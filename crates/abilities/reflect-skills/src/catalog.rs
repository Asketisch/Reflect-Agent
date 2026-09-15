//! `SkillsCatalog` —— 持有解析后的 skill 列表与激活集合。
//!
//! `pre_loop` 用它渲染 catalog,`LoadSkillTool` 用它把 skill 标记为
//! 激活态(从而把其声明的 `tools:` 暴露到 LLM 工具 schema)。

use std::collections::HashSet;
use std::path::Path;

use parking_lot::RwLock;

use crate::loader::parse_skill_file;
use crate::model::{SkillError, SkillMeta};
use crate::scanner::scan_skills_dirs;

/// 默认 "always-on" 工具名。即使没有激活任何 skill,
/// 这些工具也对 LLM 可见。
///
/// 包含 `web_fetch` / `web_search`:二者在 registry 中注册但此前不在
/// always_on,导致默认 agent 看不到 web 工具、只能退而用 `bash` + `curl`
/// 抓取原始 HTML,模型无法从 HTML 提取信息。把 web 工具纳入默认可见集。
///
/// 包含 `image_view`:多模态图片读取工具。此前不在 always_on,导致默认 agent
/// 看不到、无法解析 png/jpg 附件(图片里的代码/图表/棋盘等),只能回答
/// "I don't know"。纳入默认可见集。
///
/// v1.x Plan mode:包含 `EnterPlanMode` / `ExitPlanMode` / `PlanWrite`
/// 三个控制面工具。它们已在 ToolRegistry 注册,且 `PlanModeGate` hook
/// 把它们列入「Plan mode 下放行」白名单(设计上跨 mode 可用),但此前
/// 不在 always_on,导致 `pre_loop` 的 `effective_tools` 过滤把它们移除,
/// LLM 拿不到 function schema —— prompt 反复要求「调 PlanWrite →
/// ExitPlanMode」,LLM 却只能输出纯文本(FINAL ANSWER),触发不了
/// `tool_exec.rs` 的 plan 派发分支,不 emit `PlanReady`/`PlanDraftUpdated`,
/// TUI 也就不弹审批条、不渲染 plan。纳入 always_on 后,链路完整:
/// LLM 调 PlanWrite 写盘 → 调 ExitPlanMode → `PlanReady` → TUI 弹审批。
pub const ALWAYS_ON_TOOLS: &[&str] = &[
    "bash",
    "read",
    "write",
    "edit",
    "grep",
    "glob",
    "web_fetch",
    "web_search",
    "image_view",
    "EnterPlanMode",
    "ExitPlanMode",
    "PlanWrite",
];

/// 可用 skill 与当前激活 skill 集合的线程安全 catalog
/// (由 `LoadSkillTool::execute` 修改)。
pub struct SkillsCatalog {
    /// 技能列表;`RwLock` 以便 plugin loader 在 `Arc` 共享下追加/移除。
    skills: RwLock<Vec<SkillMeta>>,
    activated: RwLock<HashSet<String>>,
    /// 动态可追加:bootstrap 注册 `call_<role>` 子代理工具后通过
    /// [`Self::add_always_on_tools`] 补入(静态 `ALWAYS_ON_TOOLS` 无法
    /// 覆盖运行时才确定的工具名)。
    always_on: RwLock<HashSet<String>>,
    /// v1.x 功能 6:可选的 skill 白名单(非空时硬性过滤 child 可见 skill)。
    /// `None` = 不限(暴露全部 skill,向后兼容)。用于 per-subagent skill 过滤:
    /// subagent 通过 `set_allowed_skill_names` 限定自身可用的 skill 子集。
    /// 注意:仅过滤 skill,不影响 `always_on` 基础工具集。
    allowed_skills: RwLock<Option<HashSet<String>>>,
}

impl std::fmt::Debug for SkillsCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillsCatalog")
            .field("skills", &self.skills.read().len())
            .field("activated", &self.activated.read())
            .finish()
    }
}

impl SkillsCatalog {
    /// 新建空 catalog,带默认 always-on 集合。
    pub fn new() -> Self {
        Self {
            skills: RwLock::new(Vec::new()),
            activated: RwLock::new(HashSet::new()),
            always_on: RwLock::new(ALWAYS_ON_TOOLS.iter().map(|s| s.to_string()).collect()),
            allowed_skills: RwLock::new(None),
        }
    }

    /// 扫描给定目录并替换当前 skill 列表。
    /// 返回加载的 skill 数量。
    pub fn scan(&self, dirs: &[&Path]) -> usize {
        let mut skills = scan_skills_dirs(dirs);
        // 排序以保证输出确定性。
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        let n = skills.len();
        *self.skills.write() = skills;
        n
    }

    /// 插入单个 skill(测试 / 编程 API)。
    pub fn insert(&self, skill: SkillMeta) {
        self.skills.write().push(skill);
    }

    /// 从文件路径加载单个 skill 并加入 catalog。
    pub fn load_file(&self, path: &Path) -> Result<(), SkillError> {
        let mut skill = parse_skill_file(path)?;
        if skill.path.as_os_str().is_empty() {
            skill.path = path.to_path_buf();
        }
        self.skills.write().push(skill);
        Ok(())
    }

    /// 按名字查找 skill。
    pub fn get(&self, name: &str) -> Option<SkillMeta> {
        self.skills.read().iter().find(|s| s.name == name).cloned()
    }

    /// 所有 skill 名。
    pub fn names(&self) -> Vec<String> {
        self.skills.read().iter().map(|s| s.name.clone()).collect()
    }

    /// 把 skill 标记为激活。幂等。
    pub fn activate(&self, name: &str) {
        self.activated.write().insert(name.to_string());
    }

    /// 该名字的 skill 当前是否处于激活态。
    pub fn is_activated(&self, name: &str) -> bool {
        self.activated.read().contains(name)
    }

    /// 返回应对 LLM 可见的工具名集合:
    /// always-on ∪ 各已激活 skill `tools:` 列表的并集。
    /// v1.x 功能 6:`allowed_skills` 白名单非空时,仅计入白名单内 skill 的工具
    /// (always_on 基础工具不受影响)。
    /// 动态工具(如 bootstrap 注册的 `call_<role>` 子代理工具)补加入
    /// always-on 可见集。`pre_loop` 用 `active_tool_names()` 计算
    /// `effective_tools` —— 不补入的话,运行时注册的子代理工具会被从
    /// 模型请求的工具列表里滤掉,LLM 永远拿不到它们的 schema、
    /// 无法委派(离线 mock provider 无视实际工具列表回放脚本,
    /// 因此只能靠运行时真实 LLM 测试暴露)。幂等。
    pub fn add_always_on_tools(&self, names: impl IntoIterator<Item = String>) {
        let mut g = self.always_on.write();
        for n in names {
            g.insert(n);
        }
    }

    pub fn active_tool_names(&self) -> HashSet<String> {
        let mut out = self.always_on.read().clone();
        let filter = self.allowed_skills.read().clone();
        for skill_name in self.activated.read().iter() {
            // 白名单存在时跳过不在白名单内的 skill。
            if let Some(ref allow) = filter {
                if !allow.contains(skill_name.as_str()) {
                    continue;
                }
            }
            if let Some(skill) = self.get(skill_name) {
                for t in &skill.tools {
                    out.insert(t.clone());
                }
            }
        }
        out
    }

    /// 把 catalog 渲染为适合注入 system prompt 的类 Markdown 块。
    /// v1.x 功能 6:`allowed_skills` 白名单非空时只渲染白名单内 skill。
    pub fn render_for_system_prompt(&self) -> String {
        let filter = self.allowed_skills.read().clone();
        match filter {
            None => render_catalog(&self.skills.read(), &self.activated.read()),
            Some(allow) => {
                let filtered: Vec<SkillMeta> = self
                    .skills
                    .read()
                    .iter()
                    .filter(|s| allow.contains(s.name.as_str()))
                    .cloned()
                    .collect();
                render_catalog(&filtered, &self.activated.read())
            }
        }
    }

    // ── v1.x 功能 6: skill 白名单过滤(per-subagent) ───────────────────

    /// 设置 skill 白名单(非空时硬性过滤 child 可见 skill)。
    /// 供 subagent factory 在 spawn 时按 `spec.allowed_skills` 调用。
    /// 空集合视为「不限」(等价 `clear_allowed_skill_names`),暴露全部 skill。
    pub fn set_allowed_skill_names(&self, names: HashSet<String>) {
        if names.is_empty() {
            *self.allowed_skills.write() = None;
        } else {
            *self.allowed_skills.write() = Some(names);
        }
    }

    /// 清除 skill 白名单,恢复暴露全部 skill(向后兼容默认)。
    pub fn clear_allowed_skill_names(&self) {
        *self.allowed_skills.write() = None;
    }

    /// 当前 skill 白名单快照(`None` = 不限)。
    pub fn current_allowed_skill_names(&self) -> Option<HashSet<String>> {
        self.allowed_skills.read().clone()
    }

    // ── v1.0.0-rc2: plugin 命名空间 ─────────────────────────────────────

    /// 列出当前 catalog 中所有 plugin 提供的 skill 名空间(plugin_id)。
    pub fn plugin_ids(&self) -> Vec<String> {
        let mut set = std::collections::BTreeSet::new();
        for s in self.skills.read().iter() {
            if let Some(pid) = &s.plugin_id {
                set.insert(pid.clone());
            }
        }
        set.into_iter().collect()
    }

    /// 注册一组 plugin 提供的 skill。`plugin_id` 记到每个 `SkillMeta`
    /// 上,方便 `remove_plugin_skills` 反注册。
    ///
    /// `LoadedSkill` 是 reflect-plugin 的中间形态(只有 name / skill_md /
    /// description);这里转成完整 `SkillMeta`,body 用空串(Phase B 后续
    /// 让 plugin 加载器把 markdown 全文也读进来)。
    ///
    /// 命名:plugin 内 skill name 不加 namespace(同一 plugin 内不同 skills
    pub fn add_plugin_skills(&self, plugin_id: &str, loaded: &[crate::model::SkillMeta]) {
        let mut skills = self.skills.write();
        for mut s in loaded.iter().cloned() {
            s.plugin_id = Some(plugin_id.to_string());
            skills.push(s);
        }
        // 重排保持 name 字典序。
        skills.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// 移除某个 plugin 的所有 skill。返回实际移除数量。
    pub fn remove_plugin_skills(&self, plugin_id: &str) -> usize {
        let mut skills = self.skills.write();
        let before = skills.len();
        skills.retain(|s| s.plugin_id.as_deref() != Some(plugin_id));
        before - skills.len()
    }
}

impl Default for SkillsCatalog {
    fn default() -> Self {
        Self::new()
    }
}

/// 渲染 catalog 块。供 [`SkillsCatalog::render_for_system_prompt`]
/// 与 `pre_loop` 测试使用。
pub fn render_catalog(skills: &[SkillMeta], activated: &HashSet<String>) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for s in skills {
        let suffix = if !s.tools.is_empty() {
            format!(" [+tools: {}]", s.tools.join(", "))
        } else {
            String::new()
        };
        let marker = if activated.contains(&s.name) {
            " (active)"
        } else {
            ""
        };
        out.push_str(&format!(
            "- **{}**: {}{}{}\n",
            s.name, s.description, suffix, marker
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn skill(name: &str, tools: Vec<&str>) -> SkillMeta {
        SkillMeta {
            name: name.into(),
            description: format!("{name} does things"),
            triggers: vec![],
            tools: tools.into_iter().map(String::from).collect(),
            mcp_collections: vec![],
            path: PathBuf::new(),
            body: "body".into(),
            plugin_id: None,
            when_paths: vec![],
        }
    }

    #[test]
    fn render_empty_returns_empty() {
        let out = render_catalog(&[], &HashSet::new());
        assert_eq!(out, "");
    }

    #[test]
    fn render_with_tools_appends_marker() {
        let skills = vec![skill("a", vec!["read", "grep"])];
        let out = render_catalog(&skills, &HashSet::new());
        assert!(out.contains("**a**"));
        assert!(out.contains("[+tools: read, grep]"));
    }

    #[test]
    fn render_without_tools_omits_marker() {
        let skills = vec![skill("a", vec![])];
        let out = render_catalog(&skills, &HashSet::new());
        assert!(!out.contains("[+tools:"));
    }

    #[test]
    fn render_marks_active_skills() {
        let skills = vec![skill("a", vec![])];
        let mut activated = HashSet::new();
        activated.insert("a".into());
        let out = render_catalog(&skills, &activated);
        assert!(out.contains("(active)"));
    }

    #[test]
    fn active_tool_names_includes_always_on_and_activated() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("a", vec!["read", "grep"]));
        cat.insert(skill("b", vec!["write"]));
        // 尚无激活。
        let active = cat.active_tool_names();
        assert!(active.contains("bash"));
        assert!(active.contains("read"));
        // 激活 a;应加入 grep。
        cat.activate("a");
        let active = cat.active_tool_names();
        assert!(active.contains("grep"));
        // 激活 b;write 仍在(本就是 always-on)。
        cat.activate("b");
        let active = cat.active_tool_names();
        assert!(active.contains("write"));
    }

    // v1.4 子代理可见性防回归:bootstrap 注册的 `call_<role>` 工具不在静态
    // ALWAYS_ON_TOOLS 里,必须经 add_always_on_tools 动态补入后才进入可见集,
    // 否则 pre_loop 把它们从模型请求里滤掉,LLM 拿不到子代理 schema、无法委派。
    #[test]
    fn add_always_on_tools_visible_in_active_tool_names() {
        let cat = SkillsCatalog::new();
        assert!(!cat.active_tool_names().contains("call_explorer"));
        cat.add_always_on_tools(vec!["call_explorer".to_string(), "call_writer".to_string()]);
        let active = cat.active_tool_names();
        assert!(active.contains("call_explorer"));
        assert!(active.contains("call_writer"));
        // 幂等,且不影响默认 always-on 成员。
        cat.add_always_on_tools(vec!["call_explorer".to_string()]);
        let active = cat.active_tool_names();
        assert!(active.contains("bash"));
        assert!(active.contains("call_explorer"));
    }

    // v1.x Plan mode 防回归:三个 plan 控制面工具必须始终对 LLM 可见,
    // 否则 pre_loop 过滤掉它们后 LLM 拿不到 function schema,只能输出
    // 纯文本(FINAL ANSWER),触发不了 PlanReady / PlanDraftUpdated 事件,
    // TUI 不弹审批条、不渲染 plan。详见 `ALWAYS_ON_TOOLS` 文档注释。
    #[test]
    fn always_on_tools_includes_plan_control_plane() {
        for name in ["EnterPlanMode", "ExitPlanMode", "PlanWrite"] {
            assert!(
                ALWAYS_ON_TOOLS.contains(&name),
                "ALWAYS_ON_TOOLS 必须包含 {name},否则 LLM 在 Plan mode 下无法调用"
            );
        }
        // active_tool_names() 也应同步包含(SkillsCatalog::new 把常量灌进 always_on)。
        let cat = SkillsCatalog::new();
        let active = cat.active_tool_names();
        for name in ["EnterPlanMode", "ExitPlanMode", "PlanWrite"] {
            assert!(
                active.contains(name),
                "active_tool_names() 必须包含 {name},否则 pre_loop 过滤后会丢 schema"
            );
        }
    }

    #[test]
    fn activate_is_idempotent() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("a", vec![]));
        cat.activate("a");
        cat.activate("a");
        assert_eq!(cat.activated.read().len(), 1);
    }

    #[test]
    fn get_returns_skill_by_name() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("foo", vec![]));
        assert!(cat.get("foo").is_some());
        assert!(cat.get("bar").is_none());
    }

    #[test]
    fn scan_loads_skill_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("SKILL.md"),
            "---\nname: x\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        let cat = SkillsCatalog::new();
        let n = cat.scan(&[dir.path()]);
        assert_eq!(n, 1);
        assert!(cat.get("x").is_some());
    }

    #[test]
    fn render_for_system_prompt_uses_internal_state() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("x", vec!["read"]));
        cat.activate("x");
        let out = cat.render_for_system_prompt();
        assert!(out.contains("**x**"));
        assert!(out.contains("(active)"));
    }

    // ── v1.0.0-rc2: plugin 命名空间 ─────────────────────────────────────

    #[test]
    fn add_plugin_skills_records_plugin_id() {
        let cat = SkillsCatalog::new();
        let loaded = vec![skill("lint", vec![]), skill("format", vec![])];
        cat.add_plugin_skills("plugin-a", &loaded);
        assert_eq!(cat.plugin_ids(), vec!["plugin-a".to_string()]);
        let lint = cat.get("lint").unwrap();
        assert_eq!(lint.plugin_id.as_deref(), Some("plugin-a"));
    }

    #[test]
    fn remove_plugin_skills_drops_only_that_plugin() {
        let cat = SkillsCatalog::new();
        cat.add_plugin_skills("plugin-a", &[skill("lint", vec![])]);
        cat.add_plugin_skills("plugin-b", &[skill("format", vec![])]);

        let removed = cat.remove_plugin_skills("plugin-a");
        assert_eq!(removed, 1);
        assert!(cat.get("lint").is_none());
        assert!(cat.get("format").is_some());
        assert_eq!(cat.plugin_ids(), vec!["plugin-b".to_string()]);
    }

    #[test]
    fn remove_unknown_plugin_returns_zero() {
        let cat = SkillsCatalog::new();
        assert_eq!(cat.remove_plugin_skills("ghost"), 0);
    }

    #[test]
    fn builtin_skills_have_no_plugin_id() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("global-skill", vec![]));
        assert_eq!(cat.plugin_ids(), Vec::<String>::new());
    }

    // ── v1.x 功能 6: skill 白名单过滤(per-subagent) ───────────────────

    #[test]
    fn allowed_skill_filter_blocks_non_whitelisted_skill_tools() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("alpha", vec!["alpha_tool"]));
        cat.insert(skill("beta", vec!["beta_tool"]));
        cat.activate("alpha");
        cat.activate("beta");
        // 白名单只允许 alpha。
        cat.set_allowed_skill_names(["alpha".to_string()].into_iter().collect());
        let tools = cat.active_tool_names();
        assert!(tools.contains("alpha_tool"));
        assert!(!tools.contains("beta_tool"), "beta_tool 应被白名单过滤掉");
        // always_on 基础工具不受白名单影响。
        assert!(tools.contains("read"));
        assert!(tools.contains("bash"));
    }

    #[test]
    fn allowed_skill_filter_renders_only_whitelisted() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("alpha", vec![]));
        cat.insert(skill("beta", vec![]));
        cat.activate("alpha");
        cat.activate("beta");
        cat.set_allowed_skill_names(["beta".to_string()].into_iter().collect());
        let out = cat.render_for_system_prompt();
        assert!(out.contains("**beta**"));
        assert!(!out.contains("**alpha**"), "alpha 不在白名单,不应渲染");
    }

    #[test]
    fn clear_allowed_skill_names_restores_all() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("alpha", vec!["alpha_tool"]));
        cat.activate("alpha");
        cat.set_allowed_skill_names(["beta".to_string()].into_iter().collect());
        // alpha 被白名单挡住(alpha 不在 {beta})。
        assert!(!cat.active_tool_names().contains("alpha_tool"));
        cat.clear_allowed_skill_names();
        // 清除后 alpha 恢复可见。
        assert!(cat.active_tool_names().contains("alpha_tool"));
    }

    #[test]
    fn empty_whitelist_treated_as_unrestricted() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("alpha", vec!["alpha_tool"]));
        cat.activate("alpha");
        // 空集合视为「不限」(等价 clear),暴露全部 skill。
        cat.set_allowed_skill_names(HashSet::new());
        assert!(cat.current_allowed_skill_names().is_none());
        assert!(cat.active_tool_names().contains("alpha_tool"));
    }

    #[test]
    fn no_filter_default_exposes_all_skills() {
        let cat = SkillsCatalog::new();
        cat.insert(skill("alpha", vec!["alpha_tool"]));
        cat.insert(skill("beta", vec!["beta_tool"]));
        cat.activate("alpha");
        cat.activate("beta");
        assert!(cat.current_allowed_skill_names().is_none());
        let tools = cat.active_tool_names();
        assert!(tools.contains("alpha_tool"));
        assert!(tools.contains("beta_tool"));
    }
}
