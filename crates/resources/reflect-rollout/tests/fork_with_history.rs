//! Integration tests for `fork_with_history` —— fork-at-node 的核心存储原语。
//!
//! v1.2 P2:fork_with_history 改为「从父 JSONL 原样复制 records」,不再要求
//! 调用方传入 `up_to_messages`。这些测试验证:
//! - fork 真正创建子 JSONL,原样复制父会话的完整 records(user + assistant
//!   ContentBlocks + Compaction 等),无损;
//! - 双向标记 Fork 关系(子末尾 + 父末尾各一条 Fork marker);
//! - `up_to_turn_id` 截断语义;
//! - `find_session_path` 能定位子文件;
//! - 边界:空父会话、不存在父会话。

use std::io::Write;

use chrono::Utc;
use reflect_protocol::{ContentBlock, MessageRole, RolloutRecord, ThreadId, TurnId};
use reflect_rollout::index::fork_with_history;
use reflect_rollout::path::{default_base, session_path_at};
use tempfile::tempdir;

/// 同步读取一个 JSONL 文件并解析成 `Vec<RolloutRecord>`。
///
/// 不走 async `reader::replay_path`(它依赖 tokio reactor,plain `cargo test`
/// 没有 runtime)。JSONL 是 append-only 单行一记录,直接按行 `serde_json` 解析
/// 与 `replay_path` 语义一致(空行跳过,畸形行 panic —— 测试里文件是受控的)。
fn replay_file(path: &std::path::Path) -> Vec<RolloutRecord> {
    let content = std::fs::read_to_string(path).unwrap();
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<RolloutRecord>(l).unwrap())
        .collect()
}

/// 构造一个父会话 JSONL,模拟 v1.2 P2 后 `submission_loop` 实际写入的形态:
/// SessionMeta + 多个 turn(每 turn 含 user message + assistant message,
/// content 为 protocol ContentBlock 数组)+ 可选 Compaction。
///
/// `turns`:每个元素是 (user_text, assistant_text),生成一对 user/assistant
/// 消息,各自带独立 turn_id。返回 (parent_id, parent_path, Vec<turn_id>)。
fn write_parent(
    base: &std::path::Path,
    turns: &[(&str, &str)],
    summary: Option<&str>,
    model: &str,
) -> (ThreadId, std::path::PathBuf, Vec<TurnId>) {
    let sid = ThreadId::new();
    let path = session_path_at(base, sid, Utc::now());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        "{}",
        serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: sid,
            model: model.to_string(),
            started_at: Utc::now(),
            workspace: None,
        })
        .unwrap()
    )
    .unwrap();
    if let Some(s) = summary {
        writeln!(
            f,
            "{}",
            serde_json::to_string(&RolloutRecord::Compaction {
                turn_id: TurnId::new(),
                strategy: "smart_prune".to_string(),
                removed_count: 3,
                summary: s.to_string(),
            })
            .unwrap()
        )
        .unwrap();
    }
    let mut turn_ids = Vec::new();
    for (user_text, assistant_text) in turns {
        let turn_id = TurnId::new();
        turn_ids.push(turn_id);
        // user message:ContentBlock 数组(v1.2 P2 新格式)
        writeln!(
            f,
            "{}",
            serde_json::to_string(&RolloutRecord::Message {
                turn_id,
                role: MessageRole::User,
                content: serde_json::to_value(vec![ContentBlock::Text {
                    text: (*user_text).to_string(),
                }])
                .unwrap(),
            })
            .unwrap()
        )
        .unwrap();
        // assistant message:ContentBlock 数组(v1.2 P2 新格式)
        writeln!(
            f,
            "{}",
            serde_json::to_string(&RolloutRecord::Message {
                turn_id,
                role: MessageRole::Assistant,
                content: serde_json::to_value(vec![ContentBlock::Text {
                    text: (*assistant_text).to_string(),
                }])
                .unwrap(),
            })
            .unwrap()
        )
        .unwrap();
    }
    (sid, path, turn_ids)
}

/// 提取 record 里的 turn_id(仅对携带 turn_id 的 variant)。
fn record_turn_id(r: &RolloutRecord) -> Option<&TurnId> {
    match r {
        RolloutRecord::Message { turn_id, .. }
        | RolloutRecord::Compaction { turn_id, .. }
        | RolloutRecord::Checkpoint { turn_id, .. }
        | RolloutRecord::Rewind { turn_id, .. }
        | RolloutRecord::TokenCount { turn_id, .. } => Some(turn_id),
        _ => None,
    }
}

#[test]
fn fork_with_history_full_copy_preserves_all_records() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let (parent, _parent_path, _turn_ids) = write_parent(
        base,
        &[("question-1", "answer-1"), ("question-2", "answer-2")],
        Some("prior context summary"),
        "openai/gpt-4o",
    );

    // 全量复制(up_to_turn_id = None)。
    let child = fork_with_history(base, parent, "fix-branch", None).unwrap();

    // 子文件存在 + 可被 find_session_path 定位。
    let child_path = reflect_rollout::index::find_session_path(base, child);
    assert!(child_path.is_some(), "child JSONL should be discoverable");

    let records = replay_file(child_path.as_ref().unwrap());
    // 期望:SessionMeta(child) + Compaction(原样) + 2×(User+Assistant) + Fork = 7
    assert_eq!(records.len(), 7, "got {records:?}");

    // 首行是 child 的 SessionMeta(id 必须是 child,model 继承自父)。
    match &records[0] {
        RolloutRecord::SessionMeta {
            session_id, model, ..
        } => {
            assert_eq!(*session_id, child, "child SessionMeta must use child id");
            assert_eq!(model, "openai/gpt-4o");
        }
        other => panic!("expected SessionMeta first, got {other:?}"),
    }
    // 第二行:父的 Compaction 原样复制(strategy 保持 "smart_prune",非 fork_inherited)。
    assert!(matches!(
        &records[1],
        RolloutRecord::Compaction { summary, strategy, .. }
        if summary == "prior context summary" && strategy == "smart_prune"
    ));
    // 中间 4 条:2 User + 2 Assistant,原样复制(content 是 Array 非 String)。
    let msg_records: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            RolloutRecord::Message { role, content, .. } => Some((*role, content.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(msg_records.len(), 4);
    // user/assistant 交替:user, assistant, user, assistant
    assert_eq!(msg_records[0].0, MessageRole::User);
    assert_eq!(msg_records[1].0, MessageRole::Assistant);
    assert_eq!(msg_records[2].0, MessageRole::User);
    assert_eq!(msg_records[3].0, MessageRole::Assistant);
    // content 是 Array(ContentBlock 数组),不是裸 String。
    assert!(
        msg_records[0].1.is_array(),
        "user content should be Array (ContentBlocks), got {:?}",
        msg_records[0].1
    );
    // 末行 Fork 指向 parent。
    match records.last() {
        Some(RolloutRecord::Fork {
            parent_session_id,
            branch_name,
        }) => {
            assert_eq!(*parent_session_id, parent);
            assert_eq!(branch_name, "fix-branch");
        }
        other => panic!("expected Fork last, got {other:?}"),
    }
}

#[test]
fn fork_with_history_truncates_at_up_to_turn_id() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let (parent, _parent_path, turn_ids) =
        write_parent(base, &[("q1", "a1"), ("q2", "a2"), ("q3", "a3")], None, "m");

    // 截断到第 1 个 turn(含):只复制 turn_ids[0] 对应的 user + assistant。
    let child = fork_with_history(base, parent, "b", Some(&turn_ids[0])).unwrap();
    let child_path = reflect_rollout::index::find_session_path(base, child).unwrap();
    let records = replay_file(&child_path);
    // SessionMeta + 1 User + 1 Assistant + Fork = 4。
    assert_eq!(
        records.len(),
        4,
        "truncated child should have 4 records, got {records:?}"
    );
    // 截断点之后的 turn 不能出现。
    let copied_turn_ids: Vec<_> = records.iter().filter_map(record_turn_id).cloned().collect();
    assert!(
        !copied_turn_ids.contains(&turn_ids[1]) && !copied_turn_ids.contains(&turn_ids[2]),
        "records after truncation point must not be copied"
    );
}

#[test]
fn fork_with_history_missing_parent_returns_not_found() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let ghost = ThreadId::new();
    let err = fork_with_history(base, ghost, "b", None).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn fork_with_history_appends_fork_marker_to_parent() {
    let dir = tempdir().unwrap();
    let base = dir.path();
    let (parent, parent_path, _) = write_parent(base, &[("q1", "a1")], None, "m");
    // fork 前父文件:SessionMeta + 1 User + 1 Assistant = 3 行。
    assert_eq!(replay_file(&parent_path).len(), 3);

    let _child = fork_with_history(base, parent, "b", None).unwrap();

    // fork 后父文件末尾多一条 Fork marker。
    let parent_records = replay_file(&parent_path);
    assert_eq!(parent_records.len(), 4, "parent should gain a Fork marker");
    assert!(
        matches!(parent_records.last(), Some(RolloutRecord::Fork { parent_session_id, .. }) if *parent_session_id == parent)
    );
}

#[test]
fn fork_with_history_empty_parent_yields_minimal_child() {
    // 边界:无 compaction + 无 turn → 子文件只有 SessionMeta + Fork。
    let dir = tempdir().unwrap();
    let base = dir.path();
    let (parent, _, _) = write_parent(base, &[], None, "m");
    let child = fork_with_history(base, parent, "b", None).unwrap();
    let child_path = reflect_rollout::index::find_session_path(base, child).unwrap();
    let records = replay_file(&child_path);
    assert_eq!(
        records.len(),
        2,
        "minimal child = SessionMeta + Fork, got {records:?}"
    );
    assert!(matches!(records[0], RolloutRecord::SessionMeta { .. }));
    assert!(matches!(records[1], RolloutRecord::Fork { .. }));
}

/// 防回归:`default_base()` 仍指向 `$HOME/.reflect/sessions`,fork 路径不碰它。
#[test]
fn default_base_unchanged() {
    let p = default_base();
    assert!(p.ends_with(".reflect/sessions"), "got {p:?}");
}
