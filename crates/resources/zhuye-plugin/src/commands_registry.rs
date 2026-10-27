//! 插件 slash 命令注册表与用户输入展开。
//!
//! Commands capability 与其它四类不同:它不注册工具、不改变模型可见面,
//! 而是**用户输入层的 prompt 模板** —— `/demo:hello world` 这类输入在
//! 提交前被展开为 `commands/hello.md` 的正文(`$ARGUMENTS` / `$1`..`$9`
//! 替换参数)。注册表由 `loader::register` / `unregister` 维护,展开由
//! exec / serve / 门面调用方在 submit 前触发。

use std::collections::HashMap;

use parking_lot::RwLock;

use crate::capabilities::commands::LoadedCommand;

/// 全量参数占位符 —— 展开时替换为命令名后的全部参数文本。
const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";

/// 插件 slash 命令注册表 —— 以 `plugin_id` 为键整体挂/卸,按命令全名查询。
///
/// 线程安全:`Arc<CommandRegistry>` 可跨 exec / serve / reload task 共享。
#[derive(Default)]
pub struct CommandRegistry {
    entries: RwLock<HashMap<String, Vec<LoadedCommand>>>,
}

impl CommandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 挂载一个插件的全部命令(同 id 整体替换,幂等)。
    pub fn add_plugin(&self, plugin_id: &str, commands: Vec<LoadedCommand>) {
        self.entries.write().insert(plugin_id.to_string(), commands);
    }

    /// 反挂载一个插件,返回被移除的命令数。
    pub fn remove_plugin(&self, plugin_id: &str) -> usize {
        self.entries
            .write()
            .remove(plugin_id)
            .map_or(0, |cmds| cmds.len())
    }

    /// 按命令全名精确查找(`plugin:name` 或 `plugin:ns:name`)。
    pub fn lookup(&self, name: &str) -> Option<LoadedCommand> {
        self.entries
            .read()
            .values()
            .flatten()
            .find(|c| c.name == name)
            .cloned()
    }

    /// 当前已挂载的全部命令快照(跨插件,按命令名排序便于展示)。
    pub fn list(&self) -> Vec<LoadedCommand> {
        let mut all: Vec<LoadedCommand> = self.entries.read().values().flatten().cloned().collect();
        all.sort_by(|a, b| a.name.cmp(&b.name));
        all
    }
}

/// 命中命令但展开失败的原因。
#[derive(Debug, thiserror::Error)]
pub enum ExpandError {
    #[error("读取命令文件失败: {0}")]
    ReadFile(#[from] std::io::Error),
}

/// 展开成功的结果:命令全名 + 展开后的 prompt 正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedCommand {
    /// 命中的命令全名(`plugin:name` 或 `plugin:ns:name`),供
    /// `Submission::source_command` 做来源标注。
    pub name: String,
    /// 展开后的 prompt 正文(替换进 `Op::UserInput` 的 Text)。
    pub body: String,
}

/// 把形如 `/plugin:ns:name args...` 的用户输入展开为命令 markdown 正文。
///
/// 返回值语义:
/// - `None` —— 输入不是命令调用(不以 `/` 开头,或首 token 未命中任何
///   已挂载命令),调用方应原文透传;
/// - `Some(Ok(expanded))` —— 展开成功,用 `expanded.body` 替换原输入提交;
/// - `Some(Err(e))` —— 命中命令但文件读取失败,调用方应向用户报错。
pub fn expand_user_input(
    input: &str,
    registry: &CommandRegistry,
) -> Option<Result<ExpandedCommand, ExpandError>> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((n, a)) => (n, a.trim()),
        None => (rest, ""),
    };
    let command = registry.lookup(name)?;
    Some(expand_command(&command, args).map(|body| ExpandedCommand {
        name: command.name,
        body,
    }))
}

/// 展开单个命令:读 md → 剥 frontmatter → 替换参数占位符。
pub fn expand_command(command: &LoadedCommand, args: &str) -> Result<String, ExpandError> {
    let raw = std::fs::read_to_string(&command.file_path)?;
    let body = strip_frontmatter(&raw);
    Ok(substitute_args(body, args))
}

/// 剥离 YAML frontmatter(`---` 包裹的头部块);无 frontmatter 时原样返回。
fn strip_frontmatter(text: &str) -> &str {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("---") {
        return text;
    }
    let rest = &trimmed[3..];
    let Some(end) = rest.find("\n---") else {
        return text;
    };
    // 跳过收尾 `---` 所在行,正文从下一行开始。
    let after_marker = &rest[end + 1..];
    after_marker
        .split_once('\n')
        .map(|(_, body)| body)
        .unwrap_or("")
}

/// 单遍扫描替换 `$ARGUMENTS`(全量参数)与 `$1`..`$9`(位置参数)。
///
/// 单遍而非先 replace 后 replace,是为了避免参数文本里恰好含 `$1`
/// 被二次替换。`$1` 仅在后随字符不是数字时命中(不吃掉 `$12` 字面量);
/// 缺失的位置参数替换为空串。
fn substitute_args(body: &str, args: &str) -> String {
    let positionals: Vec<&str> = args.split_whitespace().collect();

    // 正文没有任何占位符且参数非空:参数以空行追加在末尾(对齐主流
    // harness 的 slash command 惯例,写入 docs/plugins.md)。
    let has_placeholder =
        body.contains("$ARGUMENTS") || (1..=9).any(|i| body.contains(&format!("${i}")));
    if !has_placeholder {
        if args.is_empty() {
            return body.to_string();
        }
        let mut out = body.to_string();
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
        out.push_str(args);
        out.push('\n');
        return out;
    }

    let mut out = String::with_capacity(body.len());
    let mut chars = body.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }
        let rest = &body[i..];
        if rest.starts_with(ARGUMENTS_PLACEHOLDER) {
            out.push_str(args);
            // `$` 已在循环中消费;这里再消费剩余的 "ARGUMENTS" 字符
            // (占位符为纯 ASCII,字符数 == 字节数)。
            for _ in 1..ARGUMENTS_PLACEHOLDER.len() {
                chars.next();
            }
            continue;
        }
        // 位置参数:$N 仅当 N 为 1..=9 且其后不是更多数字。
        let mut it = rest[1..].chars();
        if let Some(d) = it.next()
            && ('1'..='9').contains(&d)
            && !it.next().is_some_and(|n| n.is_ascii_digit())
        {
            let value = positionals
                .get(d as usize - '1' as usize)
                .copied()
                .unwrap_or("");
            out.push_str(value);
            chars.next(); // 消费数字字符
            continue;
        }
        out.push(ch);
    }
    out
}

// ── 单元测试 ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, content: &str) -> (tempfile::TempDir, LoadedCommand) {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("cmd.md");
        std::fs::write(&path, content).unwrap();
        (
            tmp,
            LoadedCommand {
                name: name.to_string(),
                file_path: path,
                description: None,
            },
        )
    }

    #[test]
    fn registry_add_lookup_remove_roundtrip() {
        let registry = CommandRegistry::new();
        let (_t1, c1) = cmd("demo:hello", "# hi");
        let (_t2, c2) = cmd("demo:utils:lint", "# lint");

        registry.add_plugin("demo", vec![c1.clone(), c2.clone()]);
        assert_eq!(registry.list().len(), 2);
        assert_eq!(registry.lookup("demo:hello").unwrap().name, "demo:hello");
        assert!(registry.lookup("demo:nope").is_none());
        // 同 id 重挂 = 整体替换(幂等)。
        registry.add_plugin("demo", vec![c1.clone()]);
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.remove_plugin("demo"), 1);
        assert_eq!(registry.remove_plugin("demo"), 0);
        assert!(registry.lookup("demo:hello").is_none());
    }

    #[test]
    fn expand_replaces_arguments_and_positionals() {
        let (_t, c) = cmd(
            "demo:greet",
            "---\ndescription: greet\n---\nHello $1 and $2!\nAll: $ARGUMENTS\n",
        );
        let out = expand_command(&c, "alice bob").unwrap();
        assert_eq!(out, "Hello alice and bob!\nAll: alice bob\n");
    }

    #[test]
    fn expand_missing_positional_becomes_empty() {
        let (_t, c) = cmd("demo:greet", "Hi $1!$2!");
        assert_eq!(expand_command(&c, "alice").unwrap(), "Hi alice!!");
    }

    #[test]
    fn expand_args_literal_dollar_not_double_replaced() {
        // 参数文本里含 `$1` 字面量时不得被二次替换。
        let (_t, c) = cmd("demo:echo", "cost: $ARGUMENTS");
        assert_eq!(expand_command(&c, "$1").unwrap(), "cost: $1");
    }

    #[test]
    fn expand_twelve_literal_survives() {
        let (_t, c) = cmd("demo:lit", "value $12 and $1");
        assert_eq!(expand_command(&c, "x y").unwrap(), "value $12 and x");
    }

    #[test]
    fn expand_appends_args_when_no_placeholder() {
        let (_t, c) = cmd("demo:plain", "# do things\n");
        assert_eq!(
            expand_command(&c, "with care").unwrap(),
            "# do things\n\nwith care\n"
        );
        // 无占位符且无参数 = 原文。
        assert_eq!(expand_command(&c, "").unwrap(), "# do things\n");
    }

    #[test]
    fn expand_user_input_passthrough_for_non_commands() {
        let registry = CommandRegistry::new();
        let (_t, c) = cmd("demo:hello", "hi $ARGUMENTS");
        registry.add_plugin("demo", vec![c]);

        // 非命令输入 / 未命中命令 → None(原文透传)。
        assert!(expand_user_input("plain question", &registry).is_none());
        assert!(expand_user_input("/unknown args", &registry).is_none());
        assert!(expand_user_input("/", &registry).is_none());

        // 命中 → 展开。
        let expanded = expand_user_input("/demo:hello world", &registry)
            .unwrap()
            .unwrap();
        assert_eq!(expanded.name, "demo:hello");
        assert_eq!(expanded.body, "hi world");
        // 无参数命令名也要能命中。
        let expanded = expand_user_input("/demo:hello", &registry)
            .unwrap()
            .unwrap();
        assert_eq!(expanded.body, "hi ");
    }

    #[test]
    fn strip_frontmatter_variants() {
        assert_eq!(
            strip_frontmatter("---\ndescription: x\n---\nbody\n"),
            "body\n"
        );
        assert_eq!(strip_frontmatter("# no fm\n"), "# no fm\n");
        assert_eq!(strip_frontmatter("---\nno closing\n"), "---\nno closing\n");
    }
}
