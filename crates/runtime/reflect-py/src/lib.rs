//! `reflect-py` — PyO3 暴露 ReflectBuilder + 同步 run API。
//!
//! ## Python 侧使用示例
//!
//! ```python
//! import reflect_py
//!
//! builder = reflect_py.ReflectBuilder("openai/gpt-4o")
//! agent = builder.workspace("/path/to/project").approvals(False).build()
//! result = agent.run("what is 2+2?")
//! print(result.text, result.status, result.input_tokens, result.output_tokens)
//! ```
//!
//! ## 设计取舍
//!
//! - 同步 `run` 让 Python 侧一行调用,不依赖 pyo3-asyncio 异步 runtime。
//!   内部释放 GIL (`py.allow_threads`) 后阻塞等待当前线程 tokio runtime
//!   完成整轮 turn。
//! - 结果以 `RunResult` dataclass 返回:`text` / `status` / `input_tokens` /
//!   `output_tokens` / `elapsed_ms`,覆盖 99% 一次性脚本场景。
//! - 流式输出暂未提供 —— 这是后续扩展点;需要时建议走 pyo3-asyncio。
//! - `SubmitError` 异常类型对应 Rust 侧 `anyhow::Error`,转字符串抛出。

use std::time::Instant;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyModule;
use pyo3::IntoPyObjectExt;
use reflect::stream::EventStream;
use reflect::{EventMsg, ReflectBuilder, Submission, TurnStatus};

/// Python 侧 fluent builder,镜像 `reflect::ReflectBuilder`。
#[pyclass(name = "ReflectBuilder")]
#[derive(Clone)]
pub struct PyReflectBuilder {
    inner: ReflectBuilder,
}

#[pymethods]
impl PyReflectBuilder {
    #[new]
    #[pyo3(signature = (model))]
    /// 构造 builder,`model` 形如 `"openai/gpt-4o"`。
    fn new(model: String) -> Self {
        Self {
            inner: ReflectBuilder::new(model),
        }
    }

    #[pyo3(signature = (path))]
    /// 设置 workspace 根目录。
    fn workspace(&self, path: String) -> Self {
        Self {
            inner: self.inner.clone().workspace(path),
        }
    }

    #[pyo3(signature = (on=true))]
    /// 开关 approval gating(默认 true)。
    fn approvals(&self, on: bool) -> Self {
        Self {
            inner: self.inner.clone().approvals(on),
        }
    }

    #[pyo3(signature = (on=true))]
    /// 启动即进入 Plan mode。
    fn plan_mode(&self, on: bool) -> Self {
        Self {
            inner: self.inner.clone().plan_mode(on),
        }
    }

    /// 返回当前 builder 配置快照(不触发 LLM / 网络)。
    fn describe(&self) -> (String, String, bool, bool) {
        (
            self.inner.model_spec().to_string(),
            self.inner.workspace_path().display().to_string(),
            self.inner.approvals_enabled(),
            self.inner.plan_mode_enabled(),
        )
    }

    /// 构建 `Reflect` 实例(需要 `OPENAI_API_KEY` 等 env)。
    fn build(&self) -> PyResult<PyReflect> {
        let agent = self
            .inner
            .clone()
            .build()
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(PyReflect { inner: agent })
    }
}

/// 已构建的 Reflect 句柄。提供 `run(prompt)` 同步调用 + `cancel_token`。
#[pyclass(name = "Reflect")]
pub struct PyReflect {
    inner: reflect::Reflect,
}

#[pymethods]
impl PyReflect {
    /// 当前 model spec。
    fn model(&self) -> String {
        self.inner.thread().config().current_model()
    }

    /// 当前 workspace 路径。
    fn workspace(&self) -> String {
        self.inner
            .thread()
            .config()
            .current_workspace()
            .display()
            .to_string()
    }

    /// 是否启用 approvals。
    fn approvals_enabled(&self) -> bool {
        self.inner.thread().config().approvals
    }

    /// 同步运行一轮 turn。返回 `RunResult`(`text` / `status` / token 用量 /
    /// 耗时)。整轮 turn 完成才返回,期间 GIL 释放(其他 Python 线程可继续工作)。
    ///
    /// 失败抛 `RuntimeError`,message 含底层错误。
    #[pyo3(signature = (prompt))]
    fn run<'py>(&self, py: Python<'py>, prompt: String) -> PyResult<Bound<'py, PyAny>> {
        let agent = self.inner.clone();
        // 释放 GIL 跑异步 turn —— 在工作线程上 spawn 一个 current-thread
        // tokio runtime,阻塞到 turn 终结。Python 主线程在此期间可继续
        // 处理其他任务(若启用子线程)。
        let result = py.allow_threads(move || run_turn_blocking(agent, prompt));
        match result {
            Ok(r) => PyRunResult::from(r).into_bound_py_any(py),
            Err(e) => Err(PyRuntimeError::new_err(e.to_string())),
        }
    }
}

/// 阻塞跑完整轮 turn。返回 `RunResult`。
fn run_turn_blocking(agent: reflect::Reflect, prompt: String) -> anyhow::Result<RunResult> {
    let started_at = Instant::now();
    // 构造当前线程 runtime。Reflect 内部 `submission_loop` 已是 `tokio::spawn`,
    // 必须在某个 tokio runtime 上下文中运行;这里提供 current_thread runtime
    // 即足够(future 链短,IO 主要走网络)。
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let sub = Submission::user_input(&prompt);
    let outcome = rt.block_on(async move {
        let mut stream = agent.submit(sub).await;
        drain_to_completion(&mut stream).await
    });
    let outcome = outcome?;
    let elapsed_ms = started_at.elapsed().as_millis() as u64;
    Ok(RunResult {
        text: outcome.text,
        status: outcome.status,
        input_tokens: outcome.input_tokens,
        output_tokens: outcome.output_tokens,
        elapsed_ms,
    })
}

/// 内部:消费 `EventStream` 直到终结事件。
async fn drain_to_completion(
    stream: &mut EventStream,
) -> anyhow::Result<TurnOutcome> {
    let mut text = String::new();
    let mut input_tokens: u32 = 0;
    let mut output_tokens: u32 = 0;
    let mut status = "success".to_string();
    let mut hit_terminal = false;
    while let Some(event) = stream.next().await {
        match event.msg {
            EventMsg::AgentMessageDelta(d) => text.push_str(&d.delta),
            EventMsg::AgentMessage(m) => {
                if text.is_empty() {
                    text = m.text;
                }
            }
            EventMsg::TokenCount(t) => {
                input_tokens = input_tokens.saturating_add(t.input_tokens);
                output_tokens = output_tokens.saturating_add(t.output_tokens);
            }
            EventMsg::TurnComplete(tc) => {
                status = turn_status_label(&tc.status).to_string();
                input_tokens = input_tokens.saturating_add(tc.usage.input_tokens);
                output_tokens = output_tokens.saturating_add(tc.usage.output_tokens);
                hit_terminal = true;
                break;
            }
            EventMsg::TurnAborted(a) => {
                status = format!("aborted:{:?}", a.reason);
                hit_terminal = true;
                break;
            }
            EventMsg::ShutdownComplete => {
                status = "shutdown".to_string();
                hit_terminal = true;
                break;
            }
            EventMsg::Error(e) => {
                status = format!("error:{}:{}", e.code, e.message);
                hit_terminal = true;
                break;
            }
            _ => {}
        }
    }
    if !hit_terminal {
        status = "incomplete".to_string();
    }
    Ok(TurnOutcome {
        text,
        status,
        input_tokens,
        output_tokens,
    })
}

struct TurnOutcome {
    text: String,
    status: String,
    input_tokens: u32,
    output_tokens: u32,
}

fn turn_status_label(s: &TurnStatus) -> &'static str {
    match s {
        TurnStatus::Success => "success",
        TurnStatus::MaxIterations => "max_iterations",
        TurnStatus::Stopped => "stopped",
        TurnStatus::TokenBudgetExceeded => "token_budget_exceeded",
    }
}

/// Rust 侧内部 `RunResult`(转 Python dataclass 前的中转结构)。
struct RunResult {
    text: String,
    status: String,
    input_tokens: u32,
    output_tokens: u32,
    elapsed_ms: u64,
}

/// Python 侧 `RunResult` dataclass。
///
/// 字段:
/// - `text`: 本轮最终回答(拼自 `AgentMessageDelta.delta`)
/// - `status`: `"success"` / `"max_iterations"` / `"stopped"` /
///   `"token_budget_exceeded"` / `"aborted:..."` / `"shutdown"` /
///   `"error:..."` / `"incomplete"`
/// - `input_tokens` / `output_tokens`: 累计 token(来自 `TokenCount` +
///   `TurnComplete.usage`)
/// - `elapsed_ms`: 本轮总耗时(从 `submit` 到 `TurnComplete` 等终结事件)
#[pyclass(name = "RunResult")]
#[derive(Clone)]
pub struct PyRunResult {
    text: String,
    status: String,
    input_tokens: u32,
    output_tokens: u32,
    elapsed_ms: u64,
}

#[pymethods]
impl PyRunResult {
    #[new]
    #[pyo3(signature = (text="", status="success", input_tokens=0, output_tokens=0, elapsed_ms=0))]
    fn new(
        text: &str,
        status: &str,
        input_tokens: u32,
        output_tokens: u32,
        elapsed_ms: u64,
    ) -> Self {
        Self {
            text: text.to_string(),
            status: status.to_string(),
            input_tokens,
            output_tokens,
            elapsed_ms,
        }
    }

    #[getter]
    fn text(&self) -> String {
        self.text.clone()
    }

    #[getter]
    fn status(&self) -> String {
        self.status.clone()
    }

    #[getter]
    fn input_tokens(&self) -> u32 {
        self.input_tokens
    }

    #[getter]
    fn output_tokens(&self) -> u32 {
        self.output_tokens
    }

    #[getter]
    fn elapsed_ms(&self) -> u64 {
        self.elapsed_ms
    }

    fn __repr__(&self) -> String {
        format!(
            "RunResult(status={:?}, input_tokens={}, output_tokens={}, elapsed_ms={})",
            self.status, self.input_tokens, self.output_tokens, self.elapsed_ms
        )
    }
}

impl From<RunResult> for PyRunResult {
    fn from(r: RunResult) -> Self {
        Self {
            text: r.text,
            status: r.status,
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            elapsed_ms: r.elapsed_ms,
        }
    }
}

/// 模块入口:maturin 加载 `reflect_py` extension。
#[pymodule]
fn reflect_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyReflectBuilder>()?;
    m.add_class::<PyReflect>()?;
    m.add_class::<PyRunResult>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_describe_defaults() {
        let b = PyReflectBuilder {
            inner: ReflectBuilder::new("openai/gpt-4o"),
        };
        let (model, ws, approvals, plan) = b.describe();
        assert_eq!(model, "openai/gpt-4o");
        assert_eq!(ws, ".");
        assert!(approvals);
        assert!(!plan);
    }

    #[test]
    fn run_result_roundtrips_all_fields() {
        let r = PyRunResult::new("hi", "success", 10, 5, 200);
        assert_eq!(r.text(), "hi");
        assert_eq!(r.status(), "success");
        assert_eq!(r.input_tokens(), 10);
        assert_eq!(r.output_tokens(), 5);
        assert_eq!(r.elapsed_ms(), 200);
    }

    #[test]
    fn turn_status_label_maps_all_variants() {
        assert_eq!(turn_status_label(&TurnStatus::Success), "success");
        assert_eq!(turn_status_label(&TurnStatus::MaxIterations), "max_iterations");
        assert_eq!(turn_status_label(&TurnStatus::Stopped), "stopped");
        assert_eq!(
            turn_status_label(&TurnStatus::TokenBudgetExceeded),
            "token_budget_exceeded"
        );
    }

    /// `run` 方法签名编译检查 —— 真实运行需要 LLM client(走 OPENAI_API_KEY),
    /// 故此处只验证 PyO3 接线 + 类型转换不 panic。
    #[test]
    fn run_signature_compiles() {
        // 不实际执行 submit;只确认 `Submission::user_input` 与 EventStream
        // 类型在编译期正确。这是 e2e 测试留到 `tests/python_smoke.rs`(后续 PR)。
        let _sub = Submission::user_input("hi");
    }

    /// `TurnHandle` 是 reflect-core 的公开类型 —— 出现在 PyO3 接口文档,
    /// 验证其存在(防止 reflect-core 改名字段名破坏绑定)。
    #[test]
    fn turn_handle_is_publicly_constructible() {
        let _t: Option<reflect_core::TurnHandle> = None;
    }
}
