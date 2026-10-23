//! `reflect init` —— 在当前目录生成 AGENTS.md 项目说明文件。
//!
//! 对齐 Claude Code / Codex CLI 的 `/init`:为 agent 提供项目上下文锚点
//! (构建命令、代码约定、目录结构)。检测常见构建工具(Rust / Node /
//! Python / Go),生成对应模板;已存在时跳过(`--force` 覆盖)。

use std::path::{Path, PathBuf};

use anyhow::Context;

/// `reflect init [--force]` 的实现。返回写入的文件路径(供调用方打印)。
pub fn run(force: bool) -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("无法获取当前目录")?;
    write_agents_md(&cwd, force)
}

/// 在 `dir` 下生成 AGENTS.md。已存在且未 `--force` 时返回
/// `Ok(已存在路径)` 并由调用方提示跳过 —— 这里不覆盖用户已维护的说明。
fn write_agents_md(dir: &Path, force: bool) -> anyhow::Result<PathBuf> {
    let target = dir.join("AGENTS.md");
    if target.exists() && !force {
        return Ok(target);
    }
    let project_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "this project".into());
    let body = render_template(&project_name, detect_stack(dir));
    std::fs::write(&target, body).with_context(|| format!("write {}", target.display()))?;
    Ok(target)
}

/// 探测的构建栈 —— 决定模板中的「常用命令」段落。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stack {
    Rust,
    Node,
    Python,
    Go,
    Generic,
}

/// 按标志文件探测构建栈(优先级:Rust > Node > Python > Go)。
fn detect_stack(dir: &Path) -> Stack {
    if dir.join("Cargo.toml").exists() {
        Stack::Rust
    } else if dir.join("package.json").exists() {
        Stack::Node
    } else if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        Stack::Python
    } else if dir.join("go.mod").exists() {
        Stack::Go
    } else {
        Stack::Generic
    }
}

/// AGENTS.md 模板。保持精简:agent 每次会话都会读到它,过长的说明
/// 只会稀释上下文 —— 用户应在占位段落里补充真正项目特定的约定。
fn render_template(project_name: &str, stack: Stack) -> String {
    let commands = match stack {
        Stack::Rust => {
            "\
# 构建 / 检查
cargo build
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace"
        }
        Stack::Node => {
            "\
# 构建 / 检查
npm install
npm run build
npm test
npm run lint"
        }
        Stack::Python => {
            "\
# 构建 / 检查
pip install -e .[dev]
pytest
ruff check ."
        }
        Stack::Go => {
            "\
# 构建 / 检查
go build ./...
go test ./...
go vet ./..."
        }
        Stack::Generic => {
            "\
# 构建 / 检查
# TODO: 补充本项目的构建、测试、lint 命令"
        }
    };
    format!(
        r#"# AGENTS.md — {project_name}

本文件是 AI agent(以及新加入的人类协作者)的项目上下文锚点。
由 `reflect init` 生成,请按项目实际情况修改;agent 每次会话启动时
自动读取本文件。

## 项目概览

TODO: 一句话说明本仓库是什么、解决什么问题。

## 常用命令

{commands}

## 代码约定

TODO: 补充本项目的硬性约定(命名、错误处理风格、注释语言等)。
示例:
- 注释与 commit message 统一使用中文
- 错误处理:库 crate 用 `thiserror`,应用层用 `anyhow`

## 目录结构

TODO: 列出关键目录及职责,例如:
- `src/` — 源码
- `tests/` — 集成测试

## 注意事项

TODO: agent 容易踩的坑(需要的外部服务、平台差异、不可动的生成文件等)。
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 首次生成:写出模板并返回路径;再次调用(无 --force)跳过不覆盖。
    #[test]
    fn writes_then_skips_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_agents_md(tmp.path(), false).unwrap();
        assert!(p.exists());
        // 手动改写内容,验证第二次调用不覆盖。
        std::fs::write(&p, "user content").unwrap();
        write_agents_md(tmp.path(), false).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "user content");
        // --force 覆盖。
        write_agents_md(tmp.path(), true).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("AGENTS.md"));
    }

    /// 栈探测:Rust 优先于其它标志文件。
    #[test]
    fn detects_stack_by_marker_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(detect_stack(tmp.path()), Stack::Generic);
        std::fs::write(tmp.path().join("go.mod"), "module x").unwrap();
        assert_eq!(detect_stack(tmp.path()), Stack::Go);
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_stack(tmp.path()), Stack::Node);
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(detect_stack(tmp.path()), Stack::Rust);
    }

    /// 模板按栈渲染对应的命令段。
    #[test]
    fn template_mentions_stack_commands() {
        assert!(render_template("demo", Stack::Rust).contains("cargo clippy"));
        assert!(render_template("demo", Stack::Node).contains("npm run build"));
        assert!(render_template("demo", Stack::Python).contains("pytest"));
        assert!(render_template("demo", Stack::Go).contains("go build"));
        assert!(render_template("demo", Stack::Generic).contains("TODO"));
    }
}
