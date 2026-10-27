//! JSONL reader / replay 辅助函数。
//!
//! 同时被 [`crate::writer::JsonlRolloutWriter::replay`](转发到
//! [`replay_path`])以及恢复 session 时的 `zhuye-exec::bootstrap_resume` 使用。

use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, BufReader};

use zhuye_protocol::{RolloutRecord, ThreadId};

/// 逐行读取 `path` 里的所有 record。
///
/// 行分类(v0.2.1 增强):
/// - 正常 record → push 到输出
/// - 未知 variant(forward-compat,旧 consumer 读 v0.3+ 写入)→ `debug!` + skip
/// - 真正 JSON 损坏(截断 / 非 JSON)→ `warn!` + skip
///
/// 未知 variant 走 debug 是为了避免 v0.2 → v0.3 升级后日志刷屏;且**不打印
/// line 内容**(可能含 prompt/工具结果),防敏感信息泄露到 stderr。
pub async fn replay_path(path: &Path) -> anyhow::Result<Vec<RolloutRecord>> {
    let mut out = Vec::new();
    if !path.exists() {
        return Ok(out);
    }
    let file = tokio::fs::File::open(path).await?;
    let reader = BufReader::new(file);
    let mut lines = reader.lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<RolloutRecord>(&line) {
            Ok(r) => out.push(r),
            Err(e) => {
                // serde_json 对未知 enum variant 的错误信息固定包含此字面量
                // (跨 platform 稳定);用于区分 forward-compat 与真损坏。
                if e.to_string().contains("unknown variant") {
                    tracing::debug!(
                        error = %e,
                        "rollout: skip unknown variant (forward-compat)"
                    );
                } else {
                    tracing::warn!(
                        error = %e,
                        "rollout: skip malformed line"
                    );
                }
            }
        }
    }
    Ok(out)
}

/// 按时间顺序读取会话的全部保留记录,包括任意日期目录和轮转副本。
pub async fn replay(base: &Path, session_id: ThreadId) -> anyhow::Result<Vec<RolloutRecord>> {
    let mut combined = Vec::new();
    for path in session_files(base, session_id)? {
        combined.extend(replay_path(&path).await?);
    }
    Ok(combined)
}

/// 同一日期内先读较旧的高编号副本,最后读活跃文件;跨日期按目录升序。
/// 同时支持文件名 thread id 和首行内部 session id 两种入口。
pub(crate) fn session_files(base: &Path, session_id: ThreadId) -> std::io::Result<Vec<PathBuf>> {
    let mut stem = session_id.to_string();
    let mut paths = crate::index::find_files_by_name_prefix(base, &stem)?;
    if paths.is_empty()
        && let Some(path) = crate::index::find_file_by_session_id(base, &session_id)?
        && let Some(name) = path.file_stem().and_then(|s| s.to_str())
    {
        stem = name.split('.').next().unwrap_or(name).to_owned();
        paths = crate::index::find_files_by_name_prefix(base, &stem)?;
    }
    let mut ranked = Vec::new();
    for path in paths {
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let rank = if name == stem {
            0
        } else if let Some(n) = name
            .strip_prefix(&format!("{stem}."))
            .and_then(|n| n.parse::<usize>().ok())
        {
            n
        } else {
            continue;
        };
        ranked.push((
            path.parent().map(Path::to_path_buf),
            std::cmp::Reverse(rank),
            path,
        ));
    }
    ranked.sort();
    Ok(ranked.into_iter().map(|(_, _, path)| path).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use zhuye_protocol::{MessageRole, TurnId};

    #[tokio::test]
    async fn roundtrips_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let mut lines = Vec::new();
        let sid = ThreadId::new();
        let tid = TurnId::new();
        let records = vec![
            RolloutRecord::session_meta(sid, "anthropic/claude"),
            RolloutRecord::message(tid, MessageRole::User, serde_json::json!("hi")),
            RolloutRecord::message(tid, MessageRole::Assistant, serde_json::json!("hello")),
            RolloutRecord::Compaction {
                turn_id: tid,
                strategy: "microcompact".into(),
                removed_count: 5,
                summary: "<summary>...</summary>".into(),
                before_tokens: 0,
                after_tokens: 0,
            },
            RolloutRecord::Fork {
                parent_session_id: sid,
                branch_name: "explorer".into(),
            },
        ];
        for r in &records {
            lines.push(serde_json::to_string(r).unwrap());
        }
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();

        let back = replay_path(&path).await.unwrap();
        assert_eq!(back.len(), records.len());
        for (a, b) in records.iter().zip(back.iter()) {
            assert_eq!(a, b);
        }
    }

    #[tokio::test]
    async fn skips_malformed_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let sid = ThreadId::new();
        let good = serde_json::to_string(&RolloutRecord::session_meta(sid, "m")).unwrap();
        let body = format!("{good}\nnot json\n\n{good}\n");
        std::fs::write(&path, body).unwrap();

        let back = replay_path(&path).await.unwrap();
        assert_eq!(back.len(), 2, "should skip 1 malformed + 1 empty");
    }

    #[tokio::test]
    async fn missing_file_returns_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        let back = replay_path(&path).await.unwrap();
        assert!(back.is_empty());
    }

    /// 未知 variant 应被静默 skip(v0.2.1 行为:走 debug log,不带 line 内容)。
    /// 模拟 v0.3+ 加新 variant,旧 consumer 读 → 不 panic,不报 warn。
    #[tokio::test]
    async fn skips_unknown_variant_silently() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let sid = ThreadId::new();
        let good = serde_json::to_string(&RolloutRecord::session_meta(sid, "m")).unwrap();
        // 模拟 v0.3 加的新 variant;serde_json 解析时报 "unknown variant"。
        let unknown = r#"{"type":"discussion_message","payload":{}}"#;
        let body = format!("{good}\n{unknown}\n{good}\n");
        std::fs::write(&path, body).unwrap();

        let back = replay_path(&path).await.unwrap();
        assert_eq!(back.len(), 2, "should skip 1 unknown variant + 2 known");
    }
}
