//! rollout 流水线的编译期常量。
//!
//! 集中放置,让 writer、脱敏层和测试共享同一组数字 —— 避免在模块间
//! 散落魔术数 `256 * 1024`。

/// 超过此字节数时活动 JSONL 文件被 rotate。
pub const ROTATE_AFTER_BYTES: u64 = 256 * 1024;

/// 最多保留此数量的 rotated 副本(`foo.1.jsonl`、`foo.2.jsonl`、
/// `foo.3.jsonl`)。更早的副本会被删除。
pub const MAX_ROTATED_FILES: usize = 3;

/// 超过此字符数时单个 JSON 字符串被截断,并附加
/// [`JSONL_REDACTION_MARKER`]。
pub const MAX_JSONL_FIELD_CHARS: usize = 16 * 1024;

/// 追加到脱敏字符串字段末尾的后缀。
pub const JSONL_REDACTION_MARKER: &str = "[redacted]";

/// 调用方未覆盖时使用的、位于 `$HOME` 下的默认基础目录。
pub const DEFAULT_ROLLOUT_DIR: &str = ".reflect/sessions";
