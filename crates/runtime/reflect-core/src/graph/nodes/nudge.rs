//! progress-nudge 辅助 + `url_host` host 抽取器(v1.x)。
//!
//! 常量与自由函数,供 `tool_exec` 检测非产出循环(重复相同的工具调用、
//! 重复抓取同一域名、迭代预算近乎耗尽),并注入结构化的
//! `<system-reminder>` 引导模型给出最终答案。

use reflect_llm::ChatMessage;

use crate::graph::state::AgentState;

/// loop-guard:同一规范化调用签名连续命中多少次后注入提醒。
pub(crate) const REPEAT_LOOP_THRESHOLD: u32 = 3;

/// progress-nudge:web_fetch 历史条目保留上限(超过则淘汰最早)。
pub(crate) const WEB_HISTORY_CAP: usize = 6;
/// progress-nudge:web_fetch 累计超过此值开始考虑注入结构化摘要。
pub(crate) const WEB_HISTORY_NUDGE_THRESHOLD: usize = 3;
/// progress-nudge:同一域名被访问达到此次数注入「信息已充分」提醒。
pub(crate) const WEB_DOMAIN_REPEAT_THRESHOLD: u32 = 2;
/// progress-nudge:迭代剩余 ≤ 此值时注入「请立即作答」强收口提示。
/// 与 `model_call` 的 max_iterations 安全阀配合:剩余 2 时 tool_exec 注入
/// nudge → model_call 让模型在最后一次迭代产出 `FINAL ANSWER:`。
pub(crate) const MAX_ITERATIONS_REMAINING_THRESHOLD: u32 = 2;

/// auto-continue:单 turn 内因「输出被 `max_tokens` 截断」而自动续作的
/// 次数上限。每次续作追加一条 User "请继续并收口" 消息并回到 `PreLoop`;
/// 续作同时也让 `iteration` +1,所以 `max_iterations` 安全阀仍是硬上限。
/// 3 次足以让需要长推理的题(如 GAIA Rubik's cube)补完作答,又不至于在
/// 模型反复啰嗦时无限烧 token。
pub(crate) const MAX_AUTO_CONTINUATIONS: u32 = 3;

/// 把一组工具调用规约为稳定签名(name + args 的 canonical JSON)。同一签名
/// 连续出现即视为「模型在重复同样的调用」。
pub(crate) fn canonical_call_signature(calls: &[reflect_tools::ToolCallRequest]) -> String {
    let mut parts: Vec<String> = calls
        .iter()
        .map(|c| {
            let args = serde_json::to_string(&c.args).unwrap_or_default();
            format!("{}({})", c.name, args)
        })
        .collect();
    parts.sort();
    parts.join("|")
}

/// 注入一条 system-reminder,提示模型停止重复调用工具、基于已有结果作答。
pub(crate) fn inject_loop_nudge(state: &mut AgentState) {
    let nudge = format!(
        "You are repeating the same tool call with identical arguments \
         without making progress. Stop calling tools. Review the results \
         you already have and give your final answer now using the \
         template: {}.",
        reflect_prompt::FINAL_ANSWER_TEMPLATE
    );
    state
        .messages
        .messages
        .push(ChatMessage::User(reflect_llm::UserContent {
            blocks: vec![reflect_llm::ContentBlock::Text {
                text: format!("<system-reminder>\n{}\n</system-reminder>", nudge),
            }],
        }));
}

/// 从 URL 中提取 host(netloc,小写)。失败时返回 None。
pub fn url_host(url: &str) -> Option<String> {
    // 避免引入 url crate:轻量解析 scheme://host/...
    let rest = url.split_once("://")?.1;
    let host = rest.split('/').next().unwrap_or(rest);
    let host = host.split('@').next_back().unwrap_or(host); // 去掉 userinfo
    let host = host.split(':').next().unwrap_or(host); // 去掉 port
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

/// v1.x progress-nudge:在 tool_exec 末尾根据 web_fetch 历史 + 域名计数
/// + 迭代剩余决定是否注入结构化提示。四个触发条件(任一即注入):
/// 0. 迭代剩余 ≤ `MAX_ITERATIONS_REMAINING_THRESHOLD` → 注入「请立即作答」
/// 1. web_fetch 次数 ≥ WEB_HISTORY_NUDGE_THRESHOLD → 注入「已收集事实」摘要
/// 2. 同一域名 ≥ WEB_DOMAIN_REPEAT_THRESHOLD 次 → 注入「信息已充分」
///
/// 与 `model_call` 的 max_iterations 安全阀配合:剩余 ≤ 2 时本轮 tool_exec
/// 注入 nudge → 回到 model_call 时模型在最后一次迭代被逼收口产出
/// `FINAL ANSWER:`,而非带着 "Let me check ..." 中途用尽迭代。
pub fn maybe_inject_progress_nudge(state: &mut AgentState, max_iterations: u32) {
    let mut nudge_lines: Vec<String> = Vec::new();

    // 触发条件 0:迭代剩余很少 → 必须立即作答(最高优先级,与 web_fetch 无关,
    // 故不再因 web_fetch_history 为空而提前 return)。
    let remaining = max_iterations.saturating_sub(state.iteration);
    if remaining <= MAX_ITERATIONS_REMAINING_THRESHOLD && max_iterations > 0 {
        nudge_lines.push(format!(
            "You are on iteration {}/{} and are about to run out of turns. \
             Stop calling tools NOW. Give your final answer immediately using the \
             template: {}. Base it on the information you \
             already have — do not attempt any further tool calls.",
            state.iteration,
            max_iterations,
            reflect_prompt::FINAL_ANSWER_TEMPLATE,
        ));
    }

    // 触发条件 1:N 次 web_fetch → 列出已抓 URL + 片段(让模型看到自己的进度)
    if !state.web_fetch_history.is_empty() {
        let n = state.web_fetch_history.len();
        if n >= WEB_HISTORY_NUDGE_THRESHOLD {
            nudge_lines.push(format!(
                "You have fetched {} web pages so far. Facts collected (URL → excerpt):",
                n
            ));
            for (i, e) in state.web_fetch_history.iter().enumerate() {
                let domain = url_host(&e.url).unwrap_or_else(|| "?".into());
                let snippet: String = e
                    .snippet
                    .chars()
                    .take(120)
                    .collect::<String>()
                    .replace('\n', " ");
                nudge_lines.push(format!("  [{}] {} — {}…", i + 1, domain, snippet));
            }
            nudge_lines.push(format!(
                "Review these facts. If you have enough to answer, stop calling tools \
                 and respond with: {}. Do NOT keep searching.",
                reflect_prompt::FINAL_ANSWER_TEMPLATE
            ));
        }

        // 触发条件 2:同域名重复 → 信息已充分
        let repeated: Vec<(&String, &u32)> = state
            .web_domain_counts
            .iter()
            .filter(|(_, c)| **c >= WEB_DOMAIN_REPEAT_THRESHOLD)
            .collect();
        if !repeated.is_empty() {
            let list = repeated
                .iter()
                .map(|(d, c)| format!("{}×{}", d, c))
                .collect::<Vec<_>>()
                .join(", ");
            nudge_lines.push(format!(
                "You have already fetched from the same domain(s) multiple times ({list}). \
                 You almost certainly have enough information — respond with {} now.",
                reflect_prompt::FINAL_ANSWER_TEMPLATE
            ));
        }
    }

    if nudge_lines.is_empty() {
        return;
    }

    let nudge = nudge_lines.join("\n");
    state
        .messages
        .messages
        .push(ChatMessage::User(reflect_llm::UserContent {
            blocks: vec![reflect_llm::ContentBlock::Text {
                text: format!("<system-reminder>\n{}\n</system-reminder>", nudge),
            }],
        }));
}
