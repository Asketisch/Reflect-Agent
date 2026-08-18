//! `reflect pipeline ...` —— v1.1.0 Phase 5 CLI:运行 DAG 流水线。
//!
//! 镜像 `reflect discussion run` 的设计 —— 读 TOML config + 调
//! `reflect_pipeline::Pipeline::from_toml` + 跑 `Pipeline::run` + 打印报告。
//!
//! ## 与 `reflect discussion run` 的区别
//!
//! - `discussion` 用 `DiscussionConfig`(顺序 / 并发 + 共识 round-based),
//!   `pipeline` 用 DAG 拓扑(任意 `depends_on`)。
//! - `discussion` 调 `DiscussionOrchestrator`;`pipeline` 调
//!   `reflect_pipeline::Pipeline`。
//! - 持久化:discussion transcript 写到 rollout JSONL;pipeline report
//!   写到 `--output` 文件(默认 stdout)。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, anyhow};
use reflect_llm::ModelRegistry;
use reflect_protocol::ThreadId;
use reflect_task::{FileTaskStore, FileTeamStore, TaskManager, TeamFile};
use reflect_tools::ToolRegistry;
use tokio_util::sync::CancellationToken;

use reflect_pipeline::{FailurePolicy, Pipeline, PipelineContext, PipelineReport};
use reflect_subagent::SubAgentFactory;

/// `reflect pipeline run -c <config.toml> --topic <topic> [--failure-policy abort|continue_collect] [--output <path>]`。
///
/// 流程:
/// 1. 读 TOML config。
/// 2. 用 `runner_for` 闭包为每个 label 选 runner(默认走 4 阶段预设)。
/// 3. 构造 `SubAgentFactory` + `TaskManager` + `CancellationToken`。
/// 4. `pipeline.run(ctx)` → `PipelineReport`。
/// 5. 打印人类可读报告,JSON 序列化写到 `--output`(可选)。
pub async fn run(
    config: &std::path::Path,
    topic: &str,
    failure_policy: Option<&str>,
    output: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    if topic.trim().is_empty() {
        return Err(anyhow!("--topic must be non-empty"));
    }
    let toml_src = std::fs::read_to_string(config)
        .with_context(|| format!("read config {}", config.display()))?;

    // 构造 manager(读 ~/.reflect 已有 team)与 factory。
    let manager = Arc::new(build_manager()?);
    let factory = Arc::new(build_factory(manager.clone()));

    // 预加载所有 team 到本地 HashMap —— `from_toml` 的 runner_for 闭包
    // 签名是同步的,无法直接 `await manager.get_team`。
    let team_index = build_team_index(&manager).await?;
    if team_index.is_empty() {
        eprintln!(
            "warn: no teams found under ~/.reflect/teams/ — \
             create teams first with `reflect task team create <name>`"
        );
    }

    // runner 闭包:根据 label 选预设。`TeamNodeRunner` 需要 team 文件,
    // 实际 `factory.add_spec` 在 runner.run 阶段完成,这里只构造 runner 实例。
    let pipeline = Pipeline::from_toml(&toml_src, |label, _params| {
        let team_name = label;
        // 4 阶段预设都映射到一个名为 `<label>` 的 team(用户事先
        // `reflect task team create` 创好)。
        let team = match team_index.get(team_name) {
            Some(t) => t.clone(),
            None => {
                eprintln!(
                    "warn: pipeline node '{label}' requires team '{team_name}' (use `reflect task team create`)"
                );
                return None;
            }
        };
        reflect_pipeline::nodes::preset_for(label, |_| Some(team.clone()))
    })?;

    let mut pipeline = pipeline;
    if let Some(p) = failure_policy {
        pipeline = pipeline.with_failure_policy(
            FailurePolicy::parse(p).map_err(|e| anyhow!("invalid --failure-policy: {e}"))?,
        );
    }

    let ctx = PipelineContext {
        topic: topic.to_string(),
        inputs: Default::default(),
        factory: factory.clone(),
        manager: manager.clone(),
        cancel: CancellationToken::new(),
        human_gate: None,
    };

    let report: PipelineReport = pipeline
        .run(ctx)
        .await
        .map_err(|e| anyhow!("pipeline run failed: {e}"))?;

    // 1. stdout:人类可读报告。
    print_report_human(&report);

    // 2. `--output` 写 JSON(若指定)。
    if let Some(out_path) = output {
        let json = serde_json::to_string_pretty(&report)?;
        std::fs::write(out_path, json)
            .with_context(|| format!("write report {}", out_path.display()))?;
        eprintln!("report written to {}", out_path.display());
    }

    // 整体 status 非 success 时,exit code = 1(便于 CI 调用)。
    if report.status != "success" {
        std::process::exit(1);
    }
    Ok(())
}

/// 构造 `SubAgentFactory` —— 不带 recorder(headless CLI 不写 rollout)。
///
/// 当前 CLI 阶段 factory 不直接接收 manager —— manager 通过 `PipelineContext`
/// 注入,factory 主要承载 `ThreadId` / model / 取消信号等元数据。
fn build_factory(_manager: Arc<TaskManager>) -> SubAgentFactory {
    let registry = Arc::new(ModelRegistry::new());
    let parent_tools = Arc::new(ToolRegistry::default());
    SubAgentFactory::new(
        ThreadId::new(),
        "openai/gpt-4o", // 默认 model(实际模型从 provider 拉,留空)
        registry,
        None, // child_registry: 回退父级 registry
        parent_tools,
        CancellationToken::new(),
        None,
    )
}

/// CLI 自带的 TaskManager —— 走 `~/.reflect/tasks/` 与 `~/.reflect/teams/`。
fn build_manager() -> anyhow::Result<TaskManager> {
    let task_store =
        FileTaskStore::with_default_home().map_err(|e| anyhow!("FileTaskStore init: {e}"))?;
    let team_store =
        FileTeamStore::with_default_home().map_err(|e| anyhow!("FileTeamStore init: {e}"))?;
    Ok(TaskManager::new(Arc::new(task_store), Arc::new(team_store)))
}

/// `reflect pipeline team <name> --topic <topic>` —— 用已注册团队跑四阶段
/// 预设 pipeline(plan → prd → exec → verify)。v1.x:接线此前孤儿的
/// `run_team_pipeline` / `TEAM_PIPELINE_TOML`。
pub async fn run_team(team_name: &str, topic: &str) -> anyhow::Result<()> {
    let manager = Arc::new(build_manager()?);
    let factory = Arc::new(build_factory(manager.clone()));
    let cancel = CancellationToken::new();
    let report =
        reflect_pipeline::run_team_pipeline(manager, team_name, topic.to_string(), factory, cancel)
            .await
            .map_err(|e| anyhow!("team pipeline failed: {e}"))?;
    let failed: Vec<_> = report
        .nodes
        .iter()
        .filter(|n| n.status != "success")
        .collect();
    println!(
        "Team pipeline '{team_name}' completed: {} ({} ms)",
        report.status, report.total_elapsed_ms
    );
    if !failed.is_empty() {
        for n in &failed {
            println!("  [{}] {}: {:?}", n.status, n.name, n.error);
        }
    }
    Ok(())
}

/// 把 manager 中所有 team 拉到一个 `HashMap<name, TeamFile>`。
///
/// `Pipeline::from_toml` 的 runner_for 闭包签名是同步的(`FnMut`),
/// 不能 `await`,因此先把 `team_store` 里的内容拍扁到内存里。
/// team 数量在实践中是 O(10),一次性加载无开销。
async fn build_team_index(manager: &TaskManager) -> anyhow::Result<HashMap<String, TeamFile>> {
    let mut idx = HashMap::new();
    for team in manager
        .list_teams()
        .await
        .map_err(|e| anyhow!("list teams: {e}"))?
    {
        idx.insert(team.name.clone(), team);
    }
    Ok(idx)
}

/// stdout 上的报告格式(非 JSON):
///
/// ```text
/// pipeline: <topic>                  # pipeline 主题
/// status: success | partial | failed # 运行状态
/// failure_policy: abort | continue_collect  # 失败策略
/// total_elapsed_ms: <ms>             # 总耗时(毫秒)
///
/// nodes:                             # 节点执行结果
///   - plan    [success]   123ms  result="Plan for: build CLI..."
///   - prd     [success]   456ms  result="PRD from plan: ..."
///   - exec    [failed ]   789ms  error="..."
/// ```
fn print_report_human(report: &PipelineReport) {
    println!("pipeline: {}", report.topic);
    println!("status: {}", report.status);
    println!("failure_policy: {}", report.failure_policy);
    println!("total_elapsed_ms: {}", report.total_elapsed_ms);
    println!();
    println!("nodes:");
    for n in &report.nodes {
        let badge = match n.status.as_str() {
            "success" => "[success]",
            "failed" => "[failed ]",
            "skipped" => "[skipped]",
            other => return println!("  - {:<8} [unknown({})]", n.name, other),
        };
        println!("  - {:<8} {} {:>6}ms", n.name, badge, n.elapsed_ms);
        if let Some(err) = &n.error {
            println!("      error: {}", err);
        } else if let Some(result) = n.outputs.get("result") {
            let result_str = match result {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let truncated = truncate_chars(&result_str, 80);
            println!(
                "      result: {}{}",
                truncated,
                if result_str.chars().count() > 80 {
                    "..."
                } else {
                    ""
                }
            );
        }
    }
}

/// 按字符截断(避免切 UTF-8 边界)。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_chars_handles_ascii_and_unicode() {
        assert_eq!(truncate_chars("hello", 3), "hel");
        assert_eq!(truncate_chars("你好世界", 2), "你好");
        // 不足 max_chars 时原样返回。
        assert_eq!(truncate_chars("hi", 10), "hi");
    }
}
