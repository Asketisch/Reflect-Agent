//! `ShellNodeRunner` — v1.4 C2:流水线 "shell" 通用节点执行器。
//!
//! 复用 `BashTool` 的执行基建(OS 沙箱 fail-closed、env 白名单、输出
//! 截断、exit code 捕获),让流水线节点可以直接跑 shell 命令 —— 典型
//! 用途:构建 / 测试 / 校验步骤(`verify_command` 之外的第一类公民)。
//!
//! ```toml
//! [nodes.build]
//! runner = "shell"
//! [nodes.build.params]
//! command = "cargo build --manifest-path {{input.repo}}/Cargo.toml"
//! timeout_secs = 300        # 可选,默认 120
//! # workspace = "/tmp/x"    # 可选,默认当前目录
//! ```
//!
//! 语义:`exit 0` → 节点 Success;非零 → 节点 Failed(stdout 仍写入
//! outputs 供下游 / 诊断),后续走向由 FailurePolicy(loop_control 可读
//! `exit_code` 做 loop 回退判定)。命令模板支持 `{{topic}}` /
//! `{{input.X}}` / `{{nodes.X.outputs.Y}}`。

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::PipelineError;
use crate::runner::{NodeContext, NodeOutcome, NodeRunner, NodeStatus};
use crate::template;
use reflect_tools::Tool as _;

/// "shell" 节点的 TOML 参数。
#[derive(Debug, Clone, Deserialize)]
pub struct ShellNodeParams {
    /// shell 命令(支持模板占位符)。
    pub command: String,
    /// 超时秒数;`None` = 120(BashTool 默认)。
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 工作目录;`None` = 当前目录。
    #[serde(default)]
    pub workspace: Option<String>,
}

/// "shell" 节点执行器。
pub struct ShellNodeRunner {
    pub name: String,
    pub command: String,
    pub timeout: Duration,
    pub workspace: std::path::PathBuf,
}

impl std::fmt::Debug for ShellNodeRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellNodeRunner")
            .field("name", &self.name)
            .field("command", &self.command)
            .field("timeout", &self.timeout)
            .field("workspace", &self.workspace)
            .finish()
    }
}

impl ShellNodeRunner {
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            timeout: Duration::from_secs(120),
            workspace: std::path::PathBuf::from("."),
        }
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    pub fn with_workspace(mut self, ws: impl Into<std::path::PathBuf>) -> Self {
        self.workspace = ws.into();
        self
    }

    /// 从 TOML params 构造(`Pipeline::from_toml` 的 runner_for 用)。
    pub fn from_params(
        name: impl Into<String>,
        params: &ShellNodeParams,
    ) -> Result<Self, PipelineError> {
        let mut r = Self::new(name, params.command.clone());
        if let Some(secs) = params.timeout_secs {
            r = r.with_timeout(secs);
        }
        if let Some(ws) = &params.workspace {
            r = r.with_workspace(ws.clone());
        }
        Ok(r)
    }
}

#[async_trait]
impl NodeRunner for ShellNodeRunner {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, ctx: &NodeContext) -> Result<NodeOutcome, PipelineError> {
        // 1. 渲染命令模板。
        let upstream: HashMap<String, serde_json::Value> = ctx.inputs.clone();
        let command = template::render(&self.command, &ctx.topic, &HashMap::new(), &upstream)
            .map_err(|e| {
                PipelineError::node_msg(self.name.clone(), format!("template render failed: {e}"))
            })?;

        // 2. 复用 BashTool 执行(env 白名单 + OS 沙箱 + 截断 + exit code)。
        //    pipeline 场景无审批 gate,直连 execute;取消令牌贯通。
        let mut tool_ctx = reflect_tools::ToolContext::for_workspace(self.workspace.clone());
        tool_ctx.timeout = self.timeout;
        tool_ctx.cancel = ctx.cancel.clone();
        let started = std::time::Instant::now();
        let output = reflect_tools::builtins::BashTool
            .execute(tool_ctx, serde_json::json!({ "cmd": command }))
            .await;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        // 3. 解析结果:exit_code 决定节点成败;stdout 始终写入 outputs。
        match output {
            Ok(out) => {
                let exit_code = out.metadata.get("exit_code").and_then(|v| v.as_i64());
                let stdout = out
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        reflect_protocol::ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let outputs = serde_json::json!({
                    "stdout": stdout,
                    "exit_code": exit_code,
                    "elapsed_ms": elapsed_ms,
                });
                let ok = exit_code == Some(0) && !out.is_error;
                if ok {
                    Ok(NodeOutcome {
                        status: NodeStatus::Success,
                        outputs,
                        error: None,
                    })
                } else {
                    Ok(NodeOutcome {
                        status: NodeStatus::Failed(format!("command exited with {exit_code:?}")),
                        outputs,
                        error: None,
                    })
                }
            }
            Err(e) => Ok(NodeOutcome::failure(format!(
                "shell node '{}' failed: {e}",
                self.name
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use reflect_subagent::SubAgentFactory;
    use reflect_task::TaskManager;
    use tokio_util::sync::CancellationToken;

    fn node_ctx() -> NodeContext {
        NodeContext {
            name: "test".into(),
            inputs: HashMap::new(),
            cancel: CancellationToken::new(),
            factory: Arc::new(SubAgentFactory::new(
                reflect_protocol::ThreadId::new(),
                "openai/gpt-4o",
                Arc::new(reflect_llm::ModelRegistry::new()),
                None,
                Arc::new(reflect_tools::ToolRegistry::default()),
                CancellationToken::new(),
                None,
            )),
            manager: test_manager(),
            topic: "t".into(),
            iteration: 1,
        }
    }

    fn test_manager() -> Arc<TaskManager> {
        Arc::new(TaskManager::new(
            Arc::new(reflect_task::InMemoryTaskStore::default()),
            Arc::new(reflect_task::InMemoryTeamStore::default()),
        ))
    }

    /// 成功命令:exit 0 → Success,stdout 进 outputs。
    #[tokio::test]
    async fn shell_runner_success_captures_stdout() {
        let r = ShellNodeRunner::new("build", "echo hello-pipeline");
        let out = r.run(&node_ctx()).await.unwrap();
        assert_eq!(out.status, NodeStatus::Success);
        assert!(
            out.outputs["stdout"]
                .as_str()
                .unwrap()
                .contains("hello-pipeline")
        );
        assert_eq!(out.outputs["exit_code"], 0);
    }

    /// 失败命令:非零 exit → Failed,stdout/exit_code 仍写入 outputs。
    #[tokio::test]
    async fn shell_runner_failure_keeps_outputs() {
        let r = ShellNodeRunner::new("check", "echo partial; exit 3");
        let out = r.run(&node_ctx()).await.unwrap();
        assert!(matches!(out.status, NodeStatus::Failed(_)));
        assert_eq!(out.outputs["exit_code"], 3);
        assert!(out.outputs["stdout"].as_str().unwrap().contains("partial"));
    }

    /// 命令模板渲染:`{{topic}}` 注入。
    #[tokio::test]
    async fn shell_runner_renders_topic_template() {
        let r = ShellNodeRunner::new("greet", "echo topic={{topic}}");
        let mut ctx = node_ctx();
        ctx.topic = "rocket-2".into();
        let out = r.run(&ctx).await.unwrap();
        assert!(
            out.outputs["stdout"]
                .as_str()
                .unwrap()
                .contains("topic=rocket-2")
        );
    }

    /// 取消令牌:取消后命令被杀,节点 Failed。
    #[tokio::test]
    async fn shell_runner_honours_cancel() {
        let cancel = CancellationToken::new();
        let mut ctx = node_ctx();
        ctx.cancel = cancel.clone();
        let r = ShellNodeRunner::new("slow", "sleep 5").with_timeout(30);
        cancel.cancel();
        let out = r.run(&ctx).await.unwrap();
        assert!(matches!(out.status, NodeStatus::Failed(_)));
    }

    /// from_params:timeout / workspace 解析。
    #[test]
    fn from_params_parses_optional_fields() {
        let params = ShellNodeParams {
            command: "ls".into(),
            timeout_secs: Some(7),
            workspace: Some("/tmp".into()),
        };
        let r = ShellNodeRunner::from_params("n", &params).unwrap();
        assert_eq!(r.timeout, Duration::from_secs(7));
        assert_eq!(r.workspace, std::path::PathBuf::from("/tmp"));

        let minimal = ShellNodeParams {
            command: "ls".into(),
            timeout_secs: None,
            workspace: None,
        };
        let r2 = ShellNodeRunner::from_params("n", &minimal).unwrap();
        assert_eq!(r2.timeout, Duration::from_secs(120));
    }
}
