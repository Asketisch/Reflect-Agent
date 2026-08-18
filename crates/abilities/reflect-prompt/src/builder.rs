//! `builder` —— `LayeredPrompt` 组合 + `PromptBuilder`。
//!
//! Reflect 的 `prompt_builder.py` 从三层组合 system prompt:
//! - **core** = agent 的 `system_prompt`(`agent.md` 的 Markdown 正文)+
//!   内存注入(上限 8KB)。这是**稳定、可缓存**层。
//! - **append** = 会话中途追加(如工具列表摘要)。当前未使用;保留以备
//!   未来 M5/M6 使用。
//! - **ephemeral** = `<system-reminder>` 块,承载实时工具列表、skills
//!   目录以及每 turn 的提醒(迭代计数、工作区路径)。**不可缓存。**
//!   注入方式见下文(v1.x 修订)。
//!
//! ## ephemeral 注入方式(v1.x 修订)
//!
//! ephemeral 现由 `pre_loop`(见 `reflect-core/src/graph/nodes/pre_loop.rs`)
//! 作为 `SystemBlock { ephemeral: true }` 追加到 `state.system_blocks`,
//! **不再**作为 trailing `User` 消息注入。此前以 User 消息注入时,
//! MiniMax-M3 等模型在多轮工具调用后会把这条 trailing `<system-reminder>`
//! 误认为「最新的用户输入」,从而遗忘原始问题、回答 "No question
//! provided"。放进 system 数组后,它属于指令上下文,不会与用户问题混淆。
//! `pre_loop.rs:191-212` 是该改动的实施位置与原因注释。
//!
//! [`inject_ephemeral_as_user`] 仍保留(有单元测试),仅用于兼容、测试与
//! 历史路径;生产路径已不走它。
//!
//! `PromptBuilder` 持有一个 [`CacheBreakDetector`],并提供单一的
//! `build_request` 辅助函数,把 `(messages, tools)` 转换为已注入
//! `cache_control` 的 `ChatRequest`。

use reflect_llm::{
    Capabilities, ChatMessage, ChatRequest, ContentBlock, SystemBlock, SystemBlocks, ToolSpec,
    UserContent,
};

use crate::caching::CacheBreakDetector;

/// 在 core system prompt 中标记内存注入段的标题。
pub const MEMORY_HEADER: &str = "## Agent Memory";

/// 在 core system prompt 中标记与 provider 无关的 Plan mode 指引的标题。
pub const PLAN_MODE_HEADER: &str = "## Plan Mode";

/// 附加到每个 agent system prompt(在 provider 序列化之前)的共享
/// Plan mode 说明。统一放在此处可避免 Anthropic / OpenAI / Ollama
/// 之间出现措辞漂移。
pub fn plan_mode_guidance() -> &'static str {
    crate::copy("system.plan-mode")
}

/// v1.x Plan mode:agent 当前处于 `PermissionMode::Plan` 时追加到
/// `core` 的"当前模式"段落。
///
/// 与 [`plan_mode_guidance`] 的差异:`plan_mode_guidance` 是「规则手册」
/// (任何时候都该记得的 EnterPlanMode/ExitPlanMode 用法),而本段是
/// 「状态告知」 —— 告诉 LLM 「你此刻正处于 Plan mode,只读、写工具被
/// 禁用,调研到能成稿就立刻 ExitPlanMode」。没有这段 LLM 不知道自己
/// 当前是否已经在规则手册说的那个 Plan mode 里,容易一直读源码而不产出
/// 计划文档。
///
/// 仍走 core 层(可缓存);只在 `PermissionMode` 切换时变。
///
/// 历史:TUI plan mode 下用户让 agent 调研代码后按 Esc,agent 没主动
/// 调 ExitPlanModeTool 出 plan —— 不是因为 Stop 路径没产出 plan,而
/// 是 LLM 不知道自己正处于 Plan mode、也不知道用户已经停了。该
/// "Active Mode" 段落连同 `pre_loop` 里的 "Previous Turn" 注入一起
/// 修复这两个盲区。
pub fn append_active_mode_section(core: &mut String, mode: reflect_protocol::PermissionMode) {
    if !matches!(mode, reflect_protocol::PermissionMode::Plan) {
        return;
    }
    // 把段插在 `## Plan Mode`(PLAN_MODE_HEADER)段之后、`## Agent Memory`
    // 段之前 —— 顺序保证 LLM 看到的是「先知道 Plan mode 规则 → 再知道
    // 现在我处于 Plan mode → 再注入 agent 记忆」。找到 PLAN_MODE_HEADER
    // 段末尾(下一个 `## ` 开头或字符串结尾)作插入点。
    const SECTION: &str = "## Active Mode\n\
        You are currently in Plan mode (read-only — `write` / `edit` are hidden from your tool list). \
        Research the codebase, then use the `PlanWrite` tool to write your complete plan as full \
        Markdown to the plan file (path shown in the per-turn reminder below). Once the plan file \
        is written, call `ExitPlanMode` to request user approval — it reads the plan from the file. \
        Do not call `ExitPlanMode` before writing the plan file. Do not keep researching once you \
        can articulate the plan. Do NOT call `EnterPlanMode` — you are already in Plan mode; calling \
        it again will trigger a redundant confirmation popup and a tool-denied error.\n";
    let plan_start = core
        .find(PLAN_MODE_HEADER)
        .expect("compose_core 一定先注入 PLAN_MODE_HEADER,这里不应缺失");
    // 在 plan_start 之后找下一个 `## ` 段开头(MEMORY_HEADER 或任何
    // 其他 ## 段)。找不到则直接 append 到 core 末尾。
    let insert_pos = core[plan_start + PLAN_MODE_HEADER.len()..]
        .find("\n## ")
        .map(|rel| plan_start + PLAN_MODE_HEADER.len() + rel + 1)
        .unwrap_or_else(|| core.len());
    let mut new = String::with_capacity(core.len() + SECTION.len());
    new.push_str(&core[..insert_pos]);
    new.push('\n');
    new.push_str(SECTION);
    if insert_pos < core.len() {
        new.push('\n');
        new.push_str(&core[insert_pos..]);
    }
    *core = new;
}

/// 包裹 ephemeral 块的前缀,让 LLM 将其识别为带外元数据
/// (而非用户撰写的内容)。
pub const EPHEMERAL_PREFIX: &str = "<system-reminder>";

/// 在诊断信息中用于标识 core(可缓存)层的前缀。
pub const CORE_PREFIX: &str = "## Core System Prompt";

/// 三层 system prompt 组合。
///
/// `core` 可缓存;`append` 保留待用;`ephemeral` 注入为 `User` 消息,
/// 永远不进缓存。
#[derive(Debug, Clone, Default)]
pub struct LayeredPrompt {
    /// 稳定、可缓存的基础(agent 的 system_prompt + 内存)。
    pub core: String,
    /// 可选的会话中途追加,未使用时为空。
    pub append: String,
    /// 每 turn 的 ephemeral 内容(`<system-reminder>` 块:实时工具列表、
    /// skills 目录、每 turn 提醒)。由 `pre_loop` 作为
    /// `SystemBlock { ephemeral: true }` 注入到 system 数组(见模块级 doc
    /// 关于 MiniMax-M3 的修订说明);**不再**作为 trailing User 消息注入。
    pub ephemeral: String,
}

impl LayeredPrompt {
    /// 构造一个空的 LayeredPrompt。
    pub fn new() -> Self {
        Self::default()
    }

    /// 设置 core 层。
    pub fn with_core(mut self, core: impl Into<String>) -> Self {
        self.core = core.into();
        self
    }

    /// 设置 append 层。
    pub fn with_append(mut self, append: impl Into<String>) -> Self {
        self.append = append.into();
        self
    }

    /// 设置 ephemeral 层。
    pub fn with_ephemeral(mut self, ephemeral: impl Into<String>) -> Self {
        self.ephemeral = ephemeral.into();
        self
    }

    /// 从 agent 的 system_prompt、共享 Plan mode 指引以及内存注入组合
    /// 出与 provider 无关的 core 层。内存追加在 `MEMORY_HEADER` 之下;
    /// 若 `memory` 为空,则整个 header 一并省略。
    pub fn compose_core(agent_system_prompt: &str, memory: &str) -> String {
        let guidance = plan_mode_guidance();
        let mut out =
            String::with_capacity(agent_system_prompt.len() + memory.len() + guidance.len() + 96);
        out.push_str(agent_system_prompt);
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(PLAN_MODE_HEADER);
        out.push('\n');
        out.push_str(guidance);
        out.push('\n');
        if !memory.trim().is_empty() {
            out.push('\n');
            out.push_str(MEMORY_HEADER);
            out.push('\n');
            out.push_str(memory.trim());
            out.push('\n');
        }
        out
    }

/// 与 [`compose_ephemeral`](Self::compose_ephemeral) 相同,但额外接收当前
    /// [`PermissionMode`](reflect_protocol::PermissionMode),用于在 Plan mode 下
    /// 改写「## Important」收尾段。
    ///
    /// **为什么需要感知模式**:原 [`compose_ephemeral`](Self::compose_ephemeral) 在
    /// 所有模式下都注入「MUST provide text final answer ... FINAL ANSWER:」指令,
    /// 这与 Plan mode 要求的「`PlanWrite` → `ExitPlanMode`」收尾路径直接冲突。弱
    /// 模型会遵循措辞绝对的 final-answer 指令,跳过 `ExitPlanMode` 直接输出文字
    /// 结论,导致 plan mode 下「不出 plan 只给结论」。Plan mode 下改写该段为 plan
    /// 专属收尾指引,从源头消除矛盾 —— 与 [`append_active_mode_section`] 的设计
    /// 动机一致。
    pub fn compose_ephemeral_with_mode(
        tools: &[ToolSpec],
        skills_catalog: &str,
        reminder: &str,
        mode: reflect_protocol::PermissionMode,
    ) -> String {
        let mut out = String::with_capacity(384);
        out.push_str(EPHEMERAL_PREFIX);
        out.push('\n');
        // 当前可用工具。
        out.push_str("## Active Tools\n");
        if tools.is_empty() {
            out.push_str("(none)\n");
        } else {
            for t in tools {
                let ToolSpec::Function { name, .. } = t;
                out.push_str("- ");
                out.push_str(&name);
                out.push('\n');
            }
        }
        out.push('\n');
        // 收尾段:Plan mode 与普通模式给出不同的「够了就停」指引。
        out.push_str("## Important\n");
        if matches!(mode, reflect_protocol::PermissionMode::Plan) {
            // Plan mode:plan 文件才是交付物。禁止用文字结论 / FINAL ANSWER 模板
            // 代替 plan —— 否则弱模型会直接输出结论而不调 ExitPlanMode。
            out.push_str(
                "You are in Plan mode. Once you have researched enough, use the `PlanWrite` \
                 tool to write your complete plan as full Markdown to the plan file, then call \
                 `ExitPlanMode` to request user approval. ",
            );
            out.push_str(
                "Do NOT output a plain-text answer in place of a plan, and do NOT use the \
                 \"FINAL ANSWER:\" template — the plan file is the deliverable. ",
            );
            out.push_str("Do not keep researching once you can articulate the plan.");
        } else {
            out.push_str("When you have gathered enough information using tools, you MUST provide a text response with your final answer. ");
            out.push_str(
                "Do NOT continue making tool calls after you have the information needed. ",
            );
            out.push_str("Always end your response with: ");
            out.push_str(crate::FINAL_ANSWER_TEMPLATE);
        }
        out.push('\n');
        out.push('\n');
        // skills 目录。
        if !skills_catalog.trim().is_empty() {
            out.push_str("## Skills\n");
            out.push_str(skills_catalog.trim());
            out.push('\n');
            out.push('\n');
        }
        // 提醒段。
        if !reminder.trim().is_empty() {
            out.push_str("## Reminder\n");
            out.push_str(reminder.trim());
            out.push('\n');
        }
        out.push_str("</system-reminder>");
        out
    }

    /// 由工具列表、skills 目录与一行 reminder(e.g. 迭代计数)拼装 ephemeral 块。
    ///
    /// 等价于以 [`reflect_protocol::PermissionMode::Auto`] 调用
    /// [`compose_ephemeral_with_mode`](Self::compose_ephemeral_with_mode),
    /// 保持向后兼容:plan mode 感知逻辑应改用新签名。
    pub fn compose_ephemeral(tools: &[ToolSpec], skills_catalog: &str, reminder: &str) -> String {
        Self::compose_ephemeral_with_mode(
            tools,
            skills_catalog,
            reminder,
            reflect_protocol::PermissionMode::Auto,
        )
    }
}

/// 持有一个 `CacheBreakDetector`,产出已注入 cache breakpoint 的
/// `ChatRequest`。除 detector 外无状态。
#[derive(Debug, Default)]
pub struct PromptBuilder {
    detector: CacheBreakDetector,
    /// v1.1.0 Phase 4:用户追加到 `core` 之后的命名 section 列表。
    /// `build_request` 时按插入顺序拼到 `core` 末尾(在 `append` 之前),
    /// 便于 coordinator prompt 等按需注入而不污染主 cacheable layer。
    /// 空 name 或空 body 在 `build_request` 跳过。
    sections: Vec<(String, String)>,
}

impl PromptBuilder {
    /// 新建带空 detector 的 builder。
    pub fn new() -> Self {
        Self::default()
    }

    /// 用既有 detector 构造。
    pub fn with_detector(detector: CacheBreakDetector) -> Self {
        Self {
            detector,
            sections: Vec::new(),
        }
    }

    /// 最近一次观测的请求 hash;从未调用过 `build_request` 时为 `None`。
    pub fn last_hash(&self) -> Option<&str> {
        self.detector.last_hash()
    }

    /// 重置 detector,让下一次 `build_request` 上报一次变化。
    pub fn reset_detector(&mut self) {
        self.detector.reset();
    }

    /// v1.1.0 Phase 4:追加一段命名 prompt section。
    ///
    /// `build_request` 时把 `(name, body)` 按追加顺序拼到 `core` 末尾
    /// (在 [`LayeredPrompt::append`] 之前),格式:
    ///
    /// ```text
    /// <core>       # 核心 prompt 内容
    ///
    /// ## <name>    # 段名(markdown 二级标题)
    /// <body>       # 段正文
    /// ```
    ///
    /// 空 `name` 或空 `body` 被静默忽略 —— 避免空 section 触发 cache miss。
    /// `name` 重复时按调用顺序全部保留(coordinator 多 section 时合理)。
    pub fn add_section(&mut self, name: impl Into<String>, body: impl Into<String>) {
        let name = name.into();
        let body = body.into();
        if name.trim().is_empty() || body.trim().is_empty() {
            return;
        }
        self.sections.push((name, body));
    }

    /// 当前已注册的 section 数量(测试 / 诊断用)。
    pub fn section_count(&self) -> usize {
        self.sections.len()
    }

    /// 按名称 upsert section:同名则替换 body,否则追加。
    /// 供 coordinator 热重载更新 `Coordinator` 段,避免重复插入。
    pub fn upsert_section(&mut self, name: impl Into<String>, body: impl Into<String>) {
        let name = name.into();
        let body = body.into();
        if name.trim().is_empty() || body.trim().is_empty() {
            return;
        }
        if let Some(entry) = self.sections.iter_mut().find(|(n, _)| *n == name) {
            entry.1 = body;
        } else {
            self.sections.push((name, body));
        }
    }

    /// 移除指定名称的 section(不存在时 no-op)。
    pub fn remove_section(&mut self, name: &str) {
        self.sections.retain(|(n, _)| n != name);
    }

    /// 由分层集合、实时消息与工具列表构造 `ChatRequest`。返回请求与
    /// 一个 `bool`(表示 cache 指纹是否变化;变化时 Anthropic 缓存
    /// 将 miss)。
    ///
    /// `caps.prompt_caching` 为 false 时请求原样返回
    /// (不注入 cache_control)。
    ///
    /// v1.1.0 Phase 4:若 `self.sections` 非空,把每段追加到 `core` 末尾
    /// (在 `append` 之前),然后走原 cache 注入路径。section 改动
    /// 同样会触发 cache fingerprint 变化,与 `core` 改写等价。
    pub fn build_request(
        &mut self,
        layers: &LayeredPrompt,
        messages: Vec<ChatMessage>,
        tools: Vec<ToolSpec>,
        model: impl Into<String>,
        caps: &Capabilities,
    ) -> (ChatRequest, bool) {
        let layers = if self.sections.is_empty() {
            layers.clone()
        } else {
            compose_with_sections(layers, &self.sections)
        };
        let mut req = ChatRequest {
            model: model.into(),
            messages,
            tools,
            system: layer_to_system_blocks(&layers),
            ..Default::default()
        };
        // 尝试注入 cache control。不支持时静默跳过。
        let injected = if caps.prompt_caching {
            crate::caching::inject_cache_control(&mut req, caps).is_ok()
        } else {
            false
        };
        let (changed, _hash) = self.detector.observe(&req);
        // `changed` 仅供信息参考;请求本身已构造正确。
        // OpenAI 路径下 `injected` 为 false,抑制 unused 告警。
        let _ = injected;
        (req, changed)
    }
}

fn layer_to_system_blocks(layers: &LayeredPrompt) -> SystemBlocks {
    let mut blocks = Vec::new();
    if !layers.core.trim().is_empty() {
        blocks.push(SystemBlock {
            text: layers.core.clone(),
            cache_control: None,
            ephemeral: false,
        });
    }
    if !layers.append.trim().is_empty() {
        blocks.push(SystemBlock {
            text: layers.append.clone(),
            cache_control: None,
            ephemeral: false,
        });
    }
    SystemBlocks(blocks)
}

/// v1.1.0 Phase 4:把 `(name, body)` section 列表追加到 `core` 末尾,
/// 返回新 `LayeredPrompt`。原 `layers.append` 在 sections 之后追加,
/// 保持原有 priority(core → sections → append → ephemeral)。
fn compose_with_sections(layers: &LayeredPrompt, sections: &[(String, String)]) -> LayeredPrompt {
    let mut core = layers.core.clone();
    if !core.is_empty() && !core.ends_with('\n') {
        core.push('\n');
    }
    for (name, body) in sections {
        if name.trim().is_empty() || body.trim().is_empty() {
            continue;
        }
        core.push('\n');
        core.push_str("## ");
        core.push_str(name.trim());
        core.push('\n');
        core.push_str(body.trim());
        core.push('\n');
    }
    LayeredPrompt {
        core,
        append: layers.append.clone(),
        ephemeral: layers.ephemeral.clone(),
    }
}

/// 把 ephemeral 块注入为末尾的 `User` 消息。
///
/// **v1.x 修订**:生产路径已不走此函数 —— `pre_loop` 现在把 ephemeral 作为
/// `SystemBlock { ephemeral: true }` 追加到 system 数组(见模块级 doc 关于
/// MiniMax-M3 的修订说明)。此函数保留用于兼容、单元测试与历史路径;新代码
/// 不应直接调用。
pub fn inject_ephemeral_as_user(req: &mut ChatRequest, ephemeral: &str) {
    if ephemeral.trim().is_empty() {
        return;
    }
    req.messages.push(ChatMessage::User(UserContent {
        blocks: vec![ContentBlock::text(ephemeral)],
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use reflect_llm::ToolSpec;
    use serde_json::json;

    #[test]
    fn compose_core_appends_memory_header() {
        let out = LayeredPrompt::compose_core("you are a reviewer", "## Facts\n- foo");
        assert!(out.contains("you are a reviewer"));
        assert!(out.contains(MEMORY_HEADER));
        assert!(out.contains("- foo"));
    }

    #[test]
    fn compose_core_includes_provider_independent_plan_mode_guidance() {
        let out = LayeredPrompt::compose_core("base", "");
        assert!(out.contains(PLAN_MODE_HEADER));
        assert!(out.contains("ExitPlanMode"));
        assert!(out.contains("write"));
        assert!(out.contains("plan 文件"));
    }

    #[test]
    fn compose_core_places_plan_guidance_before_memory() {
        let out = LayeredPrompt::compose_core("base", "remember this");
        let plan = out
            .find(PLAN_MODE_HEADER)
            .expect("Plan mode header missing");
        let memory = out.find(MEMORY_HEADER).expect("memory header missing");
        assert!(
            plan < memory,
            "shared guidance must precede agent memory: {out}"
        );
        assert_eq!(out.matches(PLAN_MODE_HEADER).count(), 1);
    }

    // ── v1.x Plan mode:append_active_mode_section 状态告知 ────────────

    /// Plan mode 下必须追加 Active Mode 段,且段内明确指引 ExitPlanMode。
    #[test]
    fn append_active_mode_section_adds_section_in_plan_mode() {
        let mut s = LayeredPrompt::compose_core("base", "");
        append_active_mode_section(&mut s, reflect_protocol::PermissionMode::Plan);
        assert!(s.contains("## Active Mode"));
        assert!(s.contains("ExitPlanMode"));
        assert!(
            s.contains("currently in Plan mode"),
            "必须明确说'currently in Plan mode'给 LLM 状态信号;got: {s}"
        );
    }

    /// 非 Plan mode( Auto / Prompt 等)下 `append_active_mode_section`
    /// 不动 core —— 不能在普通执行模式误注入「你是 Plan mode」误导 LLM。
    #[test]
    fn append_active_mode_section_noop_outside_plan_mode() {
        let original = LayeredPrompt::compose_core("base", "");
        let mut s = original.clone();
        append_active_mode_section(&mut s, reflect_protocol::PermissionMode::Auto);
        assert_eq!(s, original, "Auto mode 不能改 core");
        append_active_mode_section(&mut s, reflect_protocol::PermissionMode::Prompt);
        assert_eq!(s, original, "Prompt mode 不能改 core");
    }

    /// Active Mode 段必须出现在 Plan mode 通用规则(## Plan Mode)之后、
    /// 内存注入(## Agent Memory)之前 —— 顺序保证 LLM 看到的是「先知道
    /// Plan mode 怎么用 → 再知道现在我在 Plan mode → 再注入 agent 记忆」。
    #[test]
    fn append_active_mode_section_lands_between_plan_header_and_memory() {
        let mut s = LayeredPrompt::compose_core("base", "remember this");
        append_active_mode_section(&mut s, reflect_protocol::PermissionMode::Plan);
        let plan_header = s.find(PLAN_MODE_HEADER).expect("Plan Mode 头缺失");
        let active = s.find("## Active Mode").expect("Active Mode 头缺失");
        let memory = s.find(MEMORY_HEADER).expect("memory 头缺失");
        assert!(
            plan_header < active && active < memory,
            "顺序应为 ## Plan Mode < ## Active Mode < ## Agent Memory;got: {s}"
        );
    }

    #[test]
    fn compose_core_omits_memory_header_when_empty() {
        let out = LayeredPrompt::compose_core("base", "");
        assert!(!out.contains(MEMORY_HEADER));
        assert!(out.starts_with("base\n\n"));
        assert!(out.contains(plan_mode_guidance()));
    }

    #[test]
    fn compose_core_omits_memory_header_when_whitespace_only() {
        let out = LayeredPrompt::compose_core("base", "   \n  \n");
        assert!(!out.contains(MEMORY_HEADER));
    }

    #[test]
    fn compose_ephemeral_renders_tools_skills_reminder() {
        let tools = vec![ToolSpec::Function {
            name: "bash".into(),
            description: "shell".into(),
            parameters: json!({}),
        }];
        let out = LayeredPrompt::compose_ephemeral(&tools, "## Catalog\n- foo", "iter 1/32");
        assert!(out.starts_with(EPHEMERAL_PREFIX));
        assert!(out.contains("- bash"));
        assert!(out.contains("## Catalog"));
        assert!(out.contains("## Reminder"));
        assert!(out.contains("iter 1/32"));
        assert!(out.ends_with("</system-reminder>"));
    }

    #[test]
    fn compose_ephemeral_handles_no_tools() {
        let out = LayeredPrompt::compose_ephemeral(&[], "", "x");
        assert!(out.contains("(none)"));
    }

    #[test]
    fn compose_ephemeral_handles_no_skills_no_reminder() {
        let out = LayeredPrompt::compose_ephemeral(&[], "", "");
        assert!(out.starts_with(EPHEMERAL_PREFIX));
        assert!(out.ends_with("</system-reminder>"));
    }

    // ── compose_ephemeral_with_mode:Plan mode 收尾段分流 ──────────────

    /// Plan mode 下「## Important」段必须改写为 plan 专属收尾指引:
    /// 含 `PlanWrite` / `ExitPlanMode`,且**不含** `FINAL ANSWER` 模板 ——
    /// 否则弱模型会被 final-answer 指令带偏,直接输出文字结论而不调
    /// `ExitPlanMode`,这正是「plan mode 不出 plan 只给结论」的根因。
    #[test]
    fn compose_ephemeral_with_mode_plan_omits_final_answer_template() {
        let out = LayeredPrompt::compose_ephemeral_with_mode(
            &[],
            "",
            "iter 1/32",
            reflect_protocol::PermissionMode::Plan,
        );
        assert!(out.contains("PlanWrite"));
        assert!(out.contains("ExitPlanMode"));
        // plan 段会显式「禁止」FINAL ANSWER 模板,故输出里仍含 "FINAL ANSWER:"
        // 字样(作为禁止说明)。真正不该出现的是 final-answer 指令的特征句与
        // 完整模板占位符 `FINAL ANSWER: <answer>`。
        assert!(
            !out.contains("Always end your response with"),
            "Plan mode 不应保留 final-answer 收尾指令,got: {out}"
        );
        assert!(
            !out.contains("FINAL ANSWER: <answer>"),
            "Plan mode 不应注入 FINAL ANSWER 模板占位,got: {out}"
        );
        // 仍保留结构标签与 reminder。
        assert!(out.contains("## Active Tools"));
        assert!(out.contains("iter 1/32"));
        assert!(out.ends_with("</system-reminder>"));
    }

    /// 非 Plan mode(Auto / Prompt)保持原 final-answer 行为:含 `FINAL ANSWER`
    /// 模板。回归保护,避免普通执行模式被误改。
    #[test]
    fn compose_ephemeral_with_mode_non_plan_keeps_final_answer_template() {
        let auto = LayeredPrompt::compose_ephemeral_with_mode(
            &[],
            "",
            "",
            reflect_protocol::PermissionMode::Auto,
        );
        assert!(
            auto.contains("FINAL ANSWER"),
            "Auto 应保留 FINAL ANSWER: {auto}"
        );

        let prompt = LayeredPrompt::compose_ephemeral_with_mode(
            &[],
            "",
            "",
            reflect_protocol::PermissionMode::Prompt,
        );
        assert!(
            prompt.contains("FINAL ANSWER"),
            "Prompt 应保留 FINAL ANSWER: {prompt}"
        );
    }

    #[test]
    fn layer_to_system_blocks_skips_empty_layers() {
        let l = LayeredPrompt {
            core: "core".into(),
            append: "".into(),
            ephemeral: "ignored".into(),
        };
        let blocks = layer_to_system_blocks(&l);
        assert_eq!(blocks.0.len(), 1);
        assert_eq!(blocks.0[0].text, "core");
    }

    #[test]
    fn inject_ephemeral_as_user_skips_empty() {
        let mut req = ChatRequest::default();
        inject_ephemeral_as_user(&mut req, "");
        assert!(req.messages.is_empty());
    }

    #[test]
    fn inject_ephemeral_as_user_appends_user_message() {
        let mut req = ChatRequest::default();
        inject_ephemeral_as_user(&mut req, "<system-reminder>x</system-reminder>");
        assert_eq!(req.messages.len(), 1);
        match &req.messages[0] {
            ChatMessage::User(u) => {
                assert_eq!(u.blocks.len(), 1);
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn build_request_sets_system_blocks_from_core() {
        let mut b = PromptBuilder::new();
        let layers = LayeredPrompt {
            core: "agent says".into(),
            append: "extra".into(),
            ephemeral: String::new(),
        };
        let caps = Capabilities::default();
        let (req, changed) = b.build_request(&layers, vec![], vec![], "m", &caps);
        assert_eq!(req.system.0.len(), 2);
        assert_eq!(req.system.0[0].text, "agent says");
        assert_eq!(req.system.0[1].text, "extra");
        assert!(changed, "first observation reports changed");
    }

    #[test]
    fn build_request_reports_change_on_second_call_when_layers_differ() {
        let mut b = PromptBuilder::new();
        let caps = Capabilities::default();
        let l1 = LayeredPrompt {
            core: "v1".into(),
            ..Default::default()
        };
        let (_, c1) = b.build_request(&l1, vec![], vec![], "m", &caps);
        assert!(c1);
        let l2 = LayeredPrompt {
            core: "v2".into(),
            ..Default::default()
        };
        let (_, c2) = b.build_request(&l2, vec![], vec![], "m", &caps);
        assert!(c2, "second call with different core must report changed");
    }

    #[test]
    fn build_request_does_not_inject_cache_control_without_capability() {
        let mut b = PromptBuilder::new();
        let layers = LayeredPrompt {
            core: "x".into(),
            ..Default::default()
        };
        let caps = Capabilities::default(); // no prompt_caching
        let (req, _) = b.build_request(&layers, vec![], vec![], "m", &caps);
        assert!(req.system.0[0].cache_control.is_none());
        assert!(req.cache_control.is_empty());
    }

    #[test]
    fn build_request_injects_cache_control_with_capability() {
        let mut b = PromptBuilder::new();
        let layers = LayeredPrompt {
            core: "x".into(),
            ..Default::default()
        };
        let caps = Capabilities {
            prompt_caching: true,
            ..Capabilities::default()
        };
        let (req, _) = b.build_request(&layers, vec![], vec![], "m", &caps);
        // 最后(唯一)的 SystemBlock 带 cache_control。
        assert!(req.system.0[0].cache_control.is_some());
    }

    // ── v1.1.0 Phase 4:追加自定义段(add_section)─────────────

    /// `add_section` 注册的 section 在 `build_request` 时拼到 core 末尾。
    #[test]
    fn add_section_appends_to_core_on_build() {
        let mut b = PromptBuilder::new();
        b.add_section("Coordinator", "Custom coordinator rules.");
        assert_eq!(b.section_count(), 1);
        let layers = LayeredPrompt {
            core: "base".into(),
            ..Default::default()
        };
        let caps = Capabilities::default();
        let (req, _) = b.build_request(&layers, vec![], vec![], "m", &caps);
        // core + 1 section 注入后,SystemBlocks 应至少 1 个,合并文本含 section。
        assert!(!req.system.0.is_empty());
        let text = &req.system.0[0].text;
        assert!(text.contains("base"), "core 缺失: {text}");
        assert!(
            text.contains("## Coordinator"),
            "section header 缺失: {text}"
        );
        assert!(
            text.contains("Custom coordinator rules."),
            "section body 缺失: {text}"
        );
    }

    /// 多次 `add_section` 按插入顺序追加。
    #[test]
    fn add_section_preserves_order() {
        let mut b = PromptBuilder::new();
        b.add_section("First", "AAA");
        b.add_section("Second", "BBB");
        let layers = LayeredPrompt {
            core: "base".into(),
            ..Default::default()
        };
        let caps = Capabilities::default();
        let (req, _) = b.build_request(&layers, vec![], vec![], "m", &caps);
        let text = &req.system.0[0].text;
        let first_pos = text.find("AAA").expect("First body 缺失");
        let second_pos = text.find("BBB").expect("Second body 缺失");
        assert!(first_pos < second_pos, "顺序错误:{text}");
    }

    /// 空 name 或空 body 被静默忽略,不增加 section_count。
    #[test]
    fn add_section_ignores_empty_inputs() {
        let mut b = PromptBuilder::new();
        b.add_section("", "body");
        b.add_section("name", "");
        b.add_section("   ", "  ");
        b.add_section("ok", "   \n  \n");
        assert_eq!(b.section_count(), 0, "全部空输入应被忽略");
    }

    /// `upsert_section` 同名替换,不重复追加。
    #[test]
    fn upsert_section_replaces_same_name() {
        let mut b = PromptBuilder::new();
        b.upsert_section("Coordinator", "v1");
        b.upsert_section("Coordinator", "v2");
        assert_eq!(b.section_count(), 1);
        let layers = LayeredPrompt {
            core: "base".into(),
            ..Default::default()
        };
        let caps = Capabilities::default();
        let (req, _) = b.build_request(&layers, vec![], vec![], "m", &caps);
        assert!(req.system.0[0].text.contains("v2"));
        assert!(!req.system.0[0].text.contains("v1"));
    }

    /// `remove_section` 清掉指定段。
    #[test]
    fn remove_section_drops_named_entry() {
        let mut b = PromptBuilder::new();
        b.upsert_section("Coordinator", "rules");
        b.remove_section("Coordinator");
        assert_eq!(b.section_count(), 0);
    }

    /// 无 section 时 build_request 走原路径(`compose_with_sections` 不动 core)。
    #[test]
    fn build_request_without_sections_leaves_core_intact() {
        let mut b = PromptBuilder::new();
        let layers = LayeredPrompt {
            core: "unchanged".into(),
            ..Default::default()
        };
        let caps = Capabilities::default();
        let (req, _) = b.build_request(&layers, vec![], vec![], "m", &caps);
        assert!(req.system.0[0].text.contains("unchanged"));
        assert!(!req.system.0[0].text.contains("## "));
    }

    /// append layer 仍在 sections 之后。
    #[test]
    fn add_section_does_not_consume_append_layer() {
        let mut b = PromptBuilder::new();
        b.add_section("Coord", "rules");
        let layers = LayeredPrompt {
            core: "core".into(),
            append: "appendix".into(),
            ..Default::default()
        };
        let caps = Capabilities::default();
        let (req, _) = b.build_request(&layers, vec![], vec![], "m", &caps);
        // core+section 合并到第 1 个 block,append 是独立第 2 个 block。
        assert_eq!(req.system.0.len(), 2);
        assert!(req.system.0[0].text.contains("core"));
        assert!(req.system.0[0].text.contains("## Coord"));
        assert!(req.system.0[1].text.contains("appendix"));
    }
}
