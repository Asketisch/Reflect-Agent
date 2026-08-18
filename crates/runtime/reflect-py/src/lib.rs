//! `reflect-py` — PyO3 暴露 `ReflectBuilder` 基础 API。

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyModule;
use reflect::ReflectBuilder;

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

/// 已构建的 Reflect 句柄(P1:只读 introspection)。
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
}

/// 模块入口:maturin 加载 `reflect_py` extension。
#[pymodule]
fn reflect_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyReflectBuilder>()?;
    m.add_class::<PyReflect>()?;
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
}
