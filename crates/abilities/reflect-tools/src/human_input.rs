//! v1.5 E2:`HumanInputStore` —— `request_human_input` 的持久化存储。
//!
//! 让「等待人工输入」跨进程可见、可应答:
//!
//! - 等待开始时写挂起文件 `<dir>/<context_id>.json`
//!   (`{prompt, asked_at}`),外部进程(TUI 重启 / 其他客户端)可发现;
//! - 外部应答 = 写 `<dir>/<context_id>.answer.json`
//!   (`{"answer": "..."}`)—— 等待中的工具在轮询窗口内拿到答案立即
//!   返回,无需 TUI modal;
//! - 完成(无论 TUI 回执还是文件应答)后两份文件一并清除。
//!
//! `context_id` 经安全化处理(仅 `[A-Za-z0-9_-]`,其余折叠为 `_`,
//! 防路径逃逸);目录默认 `~/.reflect/human_input`,env
//! `REFLECT_HUMAN_INPUT_DIR` 覆盖。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 挂起请求的文件内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    pub prompt: String,
    /// Unix 毫秒。
    pub asked_at_ms: u128,
}

/// 挂起/应答文件的存储句柄。目录懒创建(首次写入时)。
#[derive(Debug, Clone)]
pub struct HumanInputStore {
    dir: PathBuf,
}

impl HumanInputStore {
    /// env `REFLECT_HUMAN_INPUT_DIR` 覆盖;默认 `~/.reflect/human_input`;
    /// HOME 缺失返回 `None`(调用方退化为无持久化)。
    pub fn from_env_or_default() -> Option<Self> {
        if let Ok(d) = std::env::var("REFLECT_HUMAN_INPUT_DIR") {
            if !d.is_empty() {
                return Some(Self {
                    dir: PathBuf::from(d),
                });
            }
        }
        std::env::var("HOME")
            .ok()
            .filter(|h| !h.is_empty())
            .map(|h| Self {
                dir: PathBuf::from(h).join(".reflect/human_input"),
            })
    }

    /// 测试 / 显式路径构造。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// context_id 安全化:仅保留 `[A-Za-z0-9_-]`,其余折叠 `_`;空串 →
    /// `"default"`。
    pub fn sanitize(context_id: &str) -> String {
        let mut s: String = context_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if s.is_empty() {
            s = "default".into();
        }
        s
    }

    fn pending_path(&self, context_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.json", Self::sanitize(context_id)))
    }

    fn answer_path(&self, context_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.answer.json", Self::sanitize(context_id)))
    }

    /// 写挂起文件(原子写:临时文件 + rename)。
    pub fn write_pending(&self, context_id: &str, prompt: &str) {
        let record = PendingRequest {
            prompt: prompt.to_string(),
            asked_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
        };
        self.write_json(&self.pending_path(context_id), &record);
    }

    /// 轮询外部应答:存在 `<id>.answer.json` 则取回答案并删除文件。
    pub fn poll_answer(&self, context_id: &str) -> Option<String> {
        let path = self.answer_path(context_id);
        let text = std::fs::read_to_string(&path).ok()?;
        #[derive(Deserialize)]
        struct Answer {
            answer: String,
        }
        let parsed: Answer = serde_json::from_str(&text).ok()?;
        let _ = std::fs::remove_file(&path);
        // 挂起文件一并清掉(已应答 = 不再挂起)。
        let _ = std::fs::remove_file(self.pending_path(context_id));
        Some(parsed.answer)
    }

    /// 清理 context 的全部文件(正常完成路径:TUI 回执后调用)。
    pub fn clear(&self, context_id: &str) {
        let _ = std::fs::remove_file(self.pending_path(context_id));
        let _ = std::fs::remove_file(self.answer_path(context_id));
    }

    /// 列出全部挂起请求(按文件名序)。供 TUI 重启回放 / `doctor`。
    pub fn list_pending(&self) -> Vec<(String, PendingRequest)> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            let Some(id) = name.strip_suffix(".json") else {
                continue;
            };
            if id.ends_with(".answer") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(record) = serde_json::from_str::<PendingRequest>(&text) {
                    out.push((id.to_string(), record));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    fn write_json(&self, path: &Path, value: &impl Serialize) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(json) = serde_json::to_string(value) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_blocks_traversal() {
        // "../../etc/passwd":三段 "../" 各折叠为 3 个 '_'。
        assert_eq!(
            HumanInputStore::sanitize("../../etc/passwd"),
            "______etc_passwd"
        );
        assert_eq!(HumanInputStore::sanitize("plan-1"), "plan-1");
        assert_eq!(HumanInputStore::sanitize(""), "default");
        assert_eq!(HumanInputStore::sanitize("../../../"), "_________");
    }

    #[test]
    fn pending_write_poll_answer_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let store = HumanInputStore::new(dir.path());

        store.write_pending("ctx-1", "请提供 API key");
        // 挂起文件可见(list_pending)。
        let pending = store.list_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, "ctx-1");
        assert_eq!(pending[0].1.prompt, "请提供 API key");

        // 外部进程应答。
        std::fs::write(
            dir.path().join("ctx-1.answer.json"),
            r#"{"answer":"sk-demo"}"#,
        )
        .unwrap();
        let answer = store.poll_answer("ctx-1").expect("应取到答案");
        assert_eq!(answer, "sk-demo");
        // 读后即焚:二次轮询为空,挂起与应答文件都消失。
        assert!(store.poll_answer("ctx-1").is_none());
        assert!(store.list_pending().is_empty());
    }

    #[test]
    fn clear_removes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = HumanInputStore::new(dir.path());
        store.write_pending("c", "p");
        std::fs::write(dir.path().join("c.answer.json"), r#"{"answer":"x"}"#).unwrap();
        store.clear("c");
        assert!(store.list_pending().is_empty());
        assert!(!dir.path().join("c.answer.json").exists());
    }
}
