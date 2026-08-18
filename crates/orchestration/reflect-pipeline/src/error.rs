//! `PipelineError` — 流水线执行期间的统一错误类型。
//!
//! 设计原则:
//! - `Cyclic` / `UnknownNode` / `DuplicateNode` 是构图期错误(同步,无 IO),
//!   一旦构造 `Pipeline` 即固定下来,不依赖运行时上下文。
//! - `Node { name, source }` 是单节点执行期错误,被 `PipelineReport` 透传给
//!   调用方,流水线本身仍能继续推进(`Abort` 策略除外)。
//! - `Template` / `Render` 是字符串模板渲染错误,出现于 `TeamNodeRunner`
//!   把 `task_template` 喂给 `SubAgentFactory::spawn` 之前。

use std::fmt;
use thiserror::Error;

/// 流水线执行期间的错误。
#[derive(Debug, Error)]
pub enum PipelineError {
    /// TOML 解析或 schema 校验失败。
    #[error("config parse: {0}")]
    Config(String),

    /// 模板字符串渲染失败(`{{input.X}}` 等占位符缺失 / 类型不匹配)。
    #[error("template render: {0}")]
    Render(String),

    /// 模板构造失败(`TeamNodeRunner::task_template` 本身的语法问题)。
    #[error("template: {0}")]
    Template(String),

    /// DAG 存在循环依赖,无法拓扑排序。
    #[error("pipeline has cycle involving node '{0}'")]
    Cyclic(String),

    /// 节点名重复。
    #[error("duplicate node name: '{0}'")]
    DuplicateNode(String),

    /// 节点名在 `depends_on` 里引用了不存在的节点。
    #[error("unknown node '{0}' referenced as dependency")]
    UnknownNode(String),

    /// `runner_for` 闭包没有为某个节点返回 runner。
    #[error("no runner for node '{0}'")]
    MissingRunner(String),

    /// 单个节点执行失败。
    #[error("node '{name}' failed: {source}")]
    Node {
        name: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    /// IO / fs / external 系统错误(写报告文件、读 config 等)。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// 顶层 `serde_yaml` / `serde_json` 序列化反序列化错误。
    #[error("serde: {0}")]
    Serde(String),
}

impl PipelineError {
    /// 构造 `Node` 错误变体,自动 box 任意 error source。
    pub fn node<E>(name: impl Into<String>, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Node {
            name: name.into(),
            source: Box::new(source),
        }
    }

    /// 字符串形式的 node 错误 —— 内部包成 `StringError` adapter,
    /// 便于 `e.to_string()` 这类非 `Error` 类型也能走 `Node` 变体。
    pub fn node_msg(name: impl Into<String>, msg: impl Into<String>) -> Self {
        let msg = msg.into();
        Self::Node {
            name: name.into(),
            source: Box::new(StringError(msg)),
        }
    }
}

/// 简单 string error wrapper —— 让任意 `String` 能塞进 `Box<dyn Error>`。
#[derive(Debug)]
struct StringError(String);
impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for StringError {}

/// `PipelineError` 的 `fmt::Display` 通过 `thiserror::Error` derive 自动派生,
/// 这里补充 `kind` accessor —— 给 `PipelineReport::failed_node_count` 等统计用。
impl PipelineError {
    /// 错误分类标签(`cyclic` / `node` / `render` 等),便于 caller 做分类处理。
    pub fn kind(&self) -> &'static str {
        match self {
            PipelineError::Config(_) => "config",
            PipelineError::Render(_) => "render",
            PipelineError::Template(_) => "template",
            PipelineError::Cyclic(_) => "cyclic",
            PipelineError::DuplicateNode(_) => "duplicate",
            PipelineError::UnknownNode(_) => "unknown",
            PipelineError::MissingRunner(_) => "missing",
            PipelineError::Node { .. } => "node",
            PipelineError::Io(_) => "io",
            PipelineError::Serde(_) => "serde",
        }
    }
}

/// 便利 trait —— 允许 `Result<T, PipelineError>` 直接 `?` 转换
/// 任意 `fmt::Display` 错误(常见于字符串拼接失败或上游 IO)。
pub trait IntoPipelineError<T> {
    fn into_pipe(self) -> Result<T, PipelineError>;
}

impl<T, E: fmt::Display> IntoPipelineError<T> for Result<T, E> {
    fn into_pipe(self) -> Result<T, PipelineError> {
        self.map_err(|e| PipelineError::Render(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_returns_distinct_labels() {
        let e = PipelineError::Cyclic("a".into());
        assert_eq!(e.kind(), "cyclic");
        let e = PipelineError::Config("bad".into());
        assert_eq!(e.kind(), "config");
    }

    #[test]
    fn node_variant_wraps_any_error() {
        let src = std::io::Error::other("boom");
        let e = PipelineError::node("plan", src);
        match e {
            PipelineError::Node { name, .. } => assert_eq!(name, "plan"),
            _ => panic!("expected Node variant"),
        }
    }
}
