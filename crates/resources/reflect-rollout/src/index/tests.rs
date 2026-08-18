//! 模块测试。从原始文件内联的 `#[cfg(test)] mod tests` 迁移而来。

use super::*;
use chrono::{TimeZone, Utc};
use reflect_protocol::{MessageRole, ThreadId, TurnId};
use tempfile::tempdir;

fn write_session(dir: &Path, day: &str, sid: ThreadId) -> std::io::Result<()> {
    let day_dir = dir.join(day);
    std::fs::create_dir_all(&day_dir)?;
    let path = day_dir.join(format!("{sid}.jsonl"));
    let mut s = String::new();
    s.push_str(&serde_json::to_string(&RolloutRecord::session_meta(sid, "openai/gpt-4o")).unwrap());
    s.push('\n');
    for i in 0..3 {
        s.push_str(
            &serde_json::to_string(&RolloutRecord::message(
                TurnId::new(),
                MessageRole::User,
                serde_json::json!(i),
            ))
            .unwrap(),
        );
        s.push('\n');
    }
    std::fs::write(path, s)
}

#[test]
fn lists_session_dirs() {
    let dir = tempdir().unwrap();
    let sid_a = ThreadId::new();
    let sid_b = ThreadId::new();
    write_session(dir.path(), "2026/06/17", sid_a).unwrap();
    write_session(dir.path(), "2026/06/18", sid_b).unwrap();

    let sessions = list_sessions(dir.path()).unwrap();
    assert_eq!(sessions.len(), 2);
    let ids: Vec<_> = sessions.iter().map(|s| s.session_id).collect();
    assert!(ids.contains(&sid_a));
    assert!(ids.contains(&sid_b));
    // 每个 session 含 3 条 message + 1 条 session_meta => 合计 4 条,3 条 message。
    for s in &sessions {
        assert_eq!(s.message_count, 3);
    }
}

/// v1.x:`list_sessions` 应从首条 User 消息派生 `title`。`write_session`
/// 写的 user 内容是 `json!(i)`(数字),`first_user_message_title` 对非
/// 字符串走 `to_string().trim_matches('"')`,所以首条 `0` → title "0"。
#[test]
fn list_sessions_derives_title_from_first_user_message() {
    let dir = tempdir().unwrap();
    let day_dir = dir.path().join("2026/06/18");
    std::fs::create_dir_all(&day_dir).unwrap();
    let sid = ThreadId::new();
    let body = format!(
        "{}\n{}\n",
        serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: sid,
            model: "m".into(),
            started_at: chrono::Utc::now(),
        })
        .unwrap(),
        serde_json::to_string(&RolloutRecord::message(
            TurnId::new(),
            MessageRole::User,
            serde_json::json!(
                "帮我修复这个 Rust 编译错误,它在 submission_loop 里报 session_id 未定义"
            ),
        ))
        .unwrap(),
    );
    std::fs::write(day_dir.join(format!("{sid}.jsonl")), body).unwrap();

    let sessions = list_sessions(dir.path()).unwrap();
    assert_eq!(sessions.len(), 1);
    let title = sessions[0].title.as_ref().expect("title should be derived");
    // 截断到 TITLE_MAX_CHARS(48)并加省略号。
    assert!(
        title.ends_with('…'),
        "long title should be truncated: {title}"
    );
    assert!(title.chars().count() == 49, "48 chars + ellipsis: {title}"); // 48 + 1
}

/// 无 User 消息的会话 → `title = None`(空会话 / 只有 SessionMeta)。
#[test]
fn list_sessions_no_title_when_no_user_message() {
    let dir = tempdir().unwrap();
    let day_dir = dir.path().join("2026/06/18");
    std::fs::create_dir_all(&day_dir).unwrap();
    let sid = ThreadId::new();
    // 只有 SessionMeta,没有任何 User 消息。
    let body = serde_json::to_string(&RolloutRecord::SessionMeta {
        session_id: sid,
        model: "m".into(),
        started_at: chrono::Utc::now(),
    })
    .unwrap();
    std::fs::write(day_dir.join(format!("{sid}.jsonl")), body).unwrap();

    let sessions = list_sessions(dir.path()).unwrap();
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].title.is_none(), "empty session has no title");
}

#[test]
fn missing_base_returns_empty() {
    let dir = tempdir().unwrap();
    let sessions = list_sessions(&dir.path().join("does/not/exist")).unwrap();
    assert!(sessions.is_empty());
}

#[test]
fn skips_files_without_session_meta() {
    let dir = tempdir().unwrap();
    let day_dir = dir.path().join("2026/06/18");
    std::fs::create_dir_all(&day_dir).unwrap();
    let bogus = day_dir.join("bogus.jsonl");
    std::fs::write(bogus, "not json\n").unwrap();
    let sessions = list_sessions(dir.path()).unwrap();
    assert!(sessions.is_empty(), "should skip bogus file");
}

/// 恢复路径:当活动 `<id>.jsonl` 的第一行不是 `SessionMeta`(rotated
/// session 在修复前的 orphan 状态)时,索引回退到同辈 rotated 文件
/// (`.1.jsonl`) 以恢复 meta,使该 session 仍出现在 `/session` 列表中。
#[test]
fn recovers_meta_from_rotated_sibling() {
    let dir = tempdir().unwrap();
    let day_dir = dir.path().join("2026/06/18");
    std::fs::create_dir_all(&day_dir).unwrap();
    let sid = ThreadId::new();
    let started = Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();

    // `.1.jsonl` 在第一行携带原始 SessionMeta。
    let rotated = day_dir.join(format!("{sid}.1.jsonl"));
    let meta_line = serde_json::to_string(&RolloutRecord::SessionMeta {
        session_id: sid,
        model: "openai/gpt-4o".into(),
        started_at: started,
    })
    .unwrap();
    std::fs::write(
        &rotated,
        format!(
            "{meta_line}\n{}\n",
            serde_json::to_string(&RolloutRecord::message(
                TurnId::new(),
                MessageRole::User,
                serde_json::json!("old"),
            ))
            .unwrap()
        ),
    )
    .unwrap();

    // 活动文件:以 Message 开头(orphan 状态),2 条消息行。
    let active = day_dir.join(format!("{sid}.jsonl"));
    let msg = serde_json::to_string(&RolloutRecord::message(
        TurnId::new(),
        MessageRole::User,
        serde_json::json!("recent"),
    ))
    .unwrap();
    std::fs::write(&active, format!("{msg}\n{msg}\n")).unwrap();

    let sessions = list_sessions(dir.path()).unwrap();
    assert_eq!(sessions.len(), 1, "should recover the orphaned session");
    assert_eq!(sessions[0].session_id, sid);
    assert_eq!(sessions[0].model, "openai/gpt-4o");
    // message_count 取自活动文件(2 行,均为消息)。
    assert_eq!(sessions[0].message_count, 2);
}

/// v0.2.4: `list_sessions_with_discussion` 只列出包含指定 `discussion_id`
/// 的 `DiscussionTranscript` record 的 session。
#[test]
fn list_sessions_with_discussion_filters_by_id() {
    use uuid::Uuid;
    let dir = tempdir().unwrap();
    let day_dir = dir.path().join("2026/06/18");
    std::fs::create_dir_all(&day_dir).unwrap();

    let target_disc = Uuid::new_v4();
    let other_disc = Uuid::new_v4();

    // Session 1: 包含 target discussion_id 的 transcript
    let sid_a = ThreadId::new();
    let path_a = day_dir.join(format!("{sid_a}.jsonl"));
    let body_a = format!(
        "{}\n{}\n",
        serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: sid_a,
            model: "openai/gpt-4o".into(),
            started_at: Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap(),
        })
        .unwrap(),
        serde_json::to_string(&RolloutRecord::DiscussionTranscript {
            discussion_id: target_disc,
            mode: "sequential".into(),
            participants: vec!["a".into(), "b".into()],
            agent_id: None,
            transcript: serde_json::json!([{"id":0,"from":"a","kind":"utterance","content":"hi"}]),
        })
        .unwrap(),
    );
    std::fs::write(&path_a, body_a).unwrap();

    // Session 2: 不包含 target,只有 other discussion_id
    let sid_b = ThreadId::new();
    let path_b = day_dir.join(format!("{sid_b}.jsonl"));
    let body_b = format!(
        "{}\n{}\n",
        serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: sid_b,
            model: "anthropic/claude".into(),
            started_at: Utc.with_ymd_and_hms(2026, 6, 18, 13, 0, 0).unwrap(),
        })
        .unwrap(),
        serde_json::to_string(&RolloutRecord::DiscussionTranscript {
            discussion_id: other_disc,
            mode: "concurrent".into(),
            participants: vec!["x".into()],
            agent_id: None,
            transcript: serde_json::json!([]),
        })
        .unwrap(),
    );
    std::fs::write(&path_b, body_b).unwrap();

    // target_disc 应只命中 sid_a
    let filtered = list_sessions_with_discussion(dir.path(), &target_disc.to_string()).unwrap();
    assert_eq!(filtered.len(), 1, "should match only sid_a");
    assert_eq!(filtered[0].session_id, sid_a);

    // other_disc 应只命中 sid_b
    let filtered = list_sessions_with_discussion(dir.path(), &other_disc.to_string()).unwrap();
    assert_eq!(filtered.len(), 1, "should match only sid_b");
    assert_eq!(filtered[0].session_id, sid_b);

    // 不存在的 uuid → 空
    let unknown = Uuid::new_v4();
    let filtered = list_sessions_with_discussion(dir.path(), &unknown.to_string()).unwrap();
    assert!(filtered.is_empty());

    // 无效的 uuid 字符串 → 静默空(trace warn)
    let filtered = list_sessions_with_discussion(dir.path(), "not-a-uuid").unwrap();
    assert!(filtered.is_empty());
}

#[test]
fn newest_first_ordering() {
    let dir = tempdir().unwrap();
    let older = Utc.with_ymd_and_hms(2026, 6, 17, 12, 0, 0).unwrap();
    let newer = Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
    let sid_old = ThreadId::new();
    let sid_new = ThreadId::new();

    // 通过 session_path_at 直接写入,显式控制 started_at。
    let path_old = crate::path::session_path_at(dir.path(), sid_old, older);
    let path_new = crate::path::session_path_at(dir.path(), sid_new, newer);
    std::fs::create_dir_all(path_old.parent().unwrap()).unwrap();
    std::fs::create_dir_all(path_new.parent().unwrap()).unwrap();
    std::fs::write(
        &path_old,
        serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: sid_old,
            model: "m".into(),
            started_at: older,
        })
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        &path_new,
        serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: sid_new,
            model: "m".into(),
            started_at: newer,
        })
        .unwrap(),
    )
    .unwrap();

    let sessions = list_sessions(dir.path()).unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].session_id, sid_new, "newest first");
    assert_eq!(sessions[1].session_id, sid_old);
}

// ── v0.4: resolve_session_index ──

/// 准备一个含 N 条 session 的 tmpdir,返回 (dir, ids_newest_to_oldest)。
fn prepare_n_sessions(n: usize) -> (tempfile::TempDir, Vec<ThreadId>) {
    let dir = tempdir().unwrap();
    let mut ids = Vec::new();
    for i in 0..n {
        let sid = ThreadId::new();
        let started = Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, i as u32).unwrap();
        let path = crate::path::session_path_at(dir.path(), sid, started);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string(&RolloutRecord::SessionMeta {
                session_id: sid,
                model: "m".into(),
                started_at: started,
            })
            .unwrap(),
        )
        .unwrap();
        ids.push(sid);
    }
    // newest first(对应 `list_sessions` 排序)
    ids.reverse();
    (dir, ids)
}

/// `-c` 选最新一条(list_sessions 已排序 newest first → [0])。
#[test]
fn resolve_session_index_continue_last_picks_newest() {
    let (dir, ids) = prepare_n_sessions(3);
    let picked = resolve_session_index(dir.path(), true, None).unwrap();
    assert_eq!(picked, ids[0]);
}

/// `-r 1` 等价 `-c`(都是最新)。
#[test]
fn resolve_session_index_resume_by_one_picks_newest() {
    let (dir, ids) = prepare_n_sessions(3);
    let picked = resolve_session_index(dir.path(), false, Some(1)).unwrap();
    assert_eq!(picked, ids[0]);
}

/// `-r 2` 选次新。
#[test]
fn resolve_session_index_resume_by_two_picks_second_newest() {
    let (dir, ids) = prepare_n_sessions(3);
    let picked = resolve_session_index(dir.path(), false, Some(2)).unwrap();
    assert_eq!(picked, ids[1]);
}

/// `-r 99` 越界 → Err(包含 "only N sessions")。
#[test]
fn resolve_session_index_resume_by_out_of_range_errors() {
    let (dir, _ids) = prepare_n_sessions(2);
    let r = resolve_session_index(dir.path(), false, Some(99));
    assert!(r.is_err());
    let msg = format!("{:#}", r.unwrap_err());
    assert!(msg.contains("only 2 sessions"), "got: {msg}");
}

/// `-r 0` → 报错(序号必须 ≥ 1)。
#[test]
fn resolve_session_index_resume_by_zero_rejected() {
    let (dir, _ids) = prepare_n_sessions(2);
    let r = resolve_session_index(dir.path(), false, Some(0));
    assert!(r.is_err());
}

/// 列表为空 → Err。
#[test]
fn resolve_session_index_empty_dir_errors() {
    let dir = tempdir().unwrap();
    let r = resolve_session_index(dir.path(), true, None);
    assert!(r.is_err());
    assert!(format!("{:#}", r.unwrap_err()).contains("no sessions found"));
}

/// 同时传 `-c` 和 `-r` → Err(互斥)。
#[test]
fn resolve_session_index_continue_last_and_resume_by_mutually_exclusive() {
    let (dir, _ids) = prepare_n_sessions(2);
    let r = resolve_session_index(dir.path(), true, Some(1));
    assert!(r.is_err());
}

/// 两者都为 None → Err(交给 caller 决定 fallback)。
#[test]
fn resolve_session_index_neither_flag_errors() {
    let (dir, _ids) = prepare_n_sessions(2);
    let r = resolve_session_index(dir.path(), false, None);
    assert!(r.is_err());
}

// ── S5b:find_session_path / rename_session / read_session_name / write_fork_record ─

/// `find_session_path` 找到 prepare_n_sessions 写下的 JSONL,SessionMeta 匹配。
#[test]
fn find_session_path_resolves_to_existing_jsonl() {
    let (dir, ids) = prepare_n_sessions(3);
    let p = find_session_path(dir.path(), ids[0]).expect("should find");
    assert!(
        p.ends_with(format!("{}.jsonl", ids[0]).as_str()),
        "got: {p:?}"
    );
    assert!(p.is_file());
}

/// `find_session_path` 找不到随机 UUID → None(不 panic)。
#[test]
fn find_session_path_returns_none_for_unknown_id() {
    let (dir, _ids) = prepare_n_sessions(2);
    let bogus = ThreadId::new();
    assert!(find_session_path(dir.path(), bogus).is_none());
}

/// `rename_session` + `read_session_name` round-trip:写后能读回,
/// 重复写(再 rename)覆盖前值。
#[test]
fn rename_session_round_trip() {
    let (dir, ids) = prepare_n_sessions(1);
    let id = ids[0];
    // 初始未命名 → read 返回 None。
    assert_eq!(read_session_name(dir.path(), id).unwrap(), None);
    // 第一次 rename。
    rename_session(dir.path(), id, "my-debug").unwrap();
    assert_eq!(
        read_session_name(dir.path(), id).unwrap().as_deref(),
        Some("my-debug")
    );
    // 第二次 rename 覆盖。
    rename_session(dir.path(), id, "production").unwrap();
    assert_eq!(
        read_session_name(dir.path(), id).unwrap().as_deref(),
        Some("production")
    );
}

/// `rename_session` 拒空名字 —— 避免写出空文件让 read 误判。
#[test]
fn rename_session_rejects_empty_name() {
    let (dir, ids) = prepare_n_sessions(1);
    assert!(rename_session(dir.path(), ids[0], "").is_err());
    assert!(rename_session(dir.path(), ids[0], "   \t  ").is_err());
    // 拒空后 _names/ 目录不应被创建(或创建了但没 .name 文件)。
    let names_dir = dir.path().join("_names");
    let entries: Vec<_> = std::fs::read_dir(&names_dir)
        .map(|rd| rd.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    for e in &entries {
        assert!(e.path().extension().and_then(|s| s.to_str()) == Some("name"));
    }
}

/// `write_fork_record` 写一条 Fork record 到 parent JSONL 末尾,
/// 返回新 ThreadId 且新 id 与 parent 不同。
#[test]
fn write_fork_record_appends_to_parent() {
    // 手工写一个**带换行**的 SessionMeta 文件;`prepare_n_sessions`
    // 不带换行,会让 fork 写入后 SessionMeta 与 fork 黏成一行,
    // `parse_first_session_meta` 失灵。
    let dir = tempdir().unwrap();
    let parent = ThreadId::new();
    let started = Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
    let path = crate::path::session_path_at(dir.path(), parent, started);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut body = String::new();
    body.push_str(
        &serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: parent,
            model: "m".into(),
            started_at: started,
        })
        .unwrap(),
    );
    body.push('\n');
    std::fs::write(&path, body).unwrap();

    let new_id = write_fork_record(dir.path(), parent, "explorer").unwrap();
    assert_ne!(new_id, parent);

    // 读回文件,扫所有 line 找 Fork record。
    let body = std::fs::read_to_string(&path).unwrap();
    let mut found_fork = false;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(RolloutRecord::Fork {
            parent_session_id,
            branch_name,
        }) = serde_json::from_str::<RolloutRecord>(line)
        {
            assert_eq!(parent_session_id, parent);
            assert_eq!(branch_name, "explorer");
            found_fork = true;
        }
    }
    assert!(found_fork, "no Fork record found in body: {body}");
}

/// `write_fork_record` parent 不存在 → Err(NotFound) 含 UUID 字符串,
/// 让 TUI Pill 友好显示。
#[test]
fn write_fork_record_errors_on_missing_parent() {
    let (dir, _ids) = prepare_n_sessions(1);
    let bogus = ThreadId::new();
    let r = write_fork_record(dir.path(), bogus, "any");
    assert!(r.is_err());
    let err = r.unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    let msg = format!("{err}");
    assert!(msg.contains(&bogus.to_string()), "err msg: {msg}");
}

/// A3:`find_checkpoint_for_turn` 精确匹配 turn_id → 返回对应 sha;
/// 未匹配 → 兜底返回最近的 checkpoint sha;无 checkpoint → None。
#[test]
fn find_checkpoint_for_turn_exact_and_fallback() {
    let (dir, ids) = prepare_n_sessions(1);
    let sid = ids[0];
    // prepare_n_sessions 写下 SessionMeta 时末尾不带换行,
    // 这里补一个换行,让后续 append 的 Checkpoint 行落到新行。
    if let Some(p) = find_session_path(dir.path(), sid) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f).unwrap();
    }
    let turn_a = TurnId::new();
    let turn_b = TurnId::new();
    let r1 = write_checkpoint_record(dir.path(), sid, turn_a, "shaAAA", None);
    assert!(r1.is_ok(), "first write failed: {r1:?}");
    let r2 = write_checkpoint_record(dir.path(), sid, turn_b, "shaBBB", None);
    assert!(r2.is_ok(), "second write failed: {r2:?}");

    let got = find_checkpoint_for_turn(dir.path(), sid, &turn_a).unwrap();
    assert_eq!(got, "shaAAA");
    let got = find_checkpoint_for_turn(dir.path(), sid, &turn_b).unwrap();
    assert_eq!(got, "shaBBB");
    let unknown = TurnId::new();
    let got = find_checkpoint_for_turn(dir.path(), sid, &unknown).unwrap();
    assert_eq!(got, "shaBBB");
}

#[test]
fn find_checkpoint_for_turn_none_when_no_checkpoints() {
    let (dir, ids) = prepare_n_sessions(1);
    let sid = ids[0];
    let t = TurnId::new();
    assert_eq!(find_checkpoint_for_turn(dir.path(), sid, &t), None);
}

// ── v1.x:TokenCount 聚合测试 ────────────────────────────────────────

use reflect_protocol::TokenUsage;

/// 写一条 `RolloutRecord::TokenCount` 单行 JSONL。
fn token_count_line(usage: TokenUsage, cost_usd: Option<f64>) -> String {
    serde_json::to_string(&RolloutRecord::TokenCount {
        turn_id: TurnId::new(),
        usage,
        cost_usd,
        at: chrono::Utc::now(),
    })
    .unwrap()
}

/// v1.x:`list_sessions` 应从每条 `TokenCount` 记录聚合 input / output /
/// total 三项(`saturating_add` 累加)。
#[test]
fn list_sessions_aggregates_token_count_from_records() {
    let dir = tempdir().unwrap();
    let day_dir = dir.path().join("2026/07/31");
    std::fs::create_dir_all(&day_dir).unwrap();
    let sid = ThreadId::new();

    let mut body = String::new();
    body.push_str(
        &serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: sid,
            model: "openai/gpt-4o".into(),
            started_at: chrono::Utc::now(),
        })
        .unwrap(),
    );
    body.push('\n');
    // 3 条 TokenCount:input 1000/200/300,output 200/50/100,total 1200/250/400。
    for (i, o, t) in [(1000u32, 200u32, 1200u32), (200, 50, 250), (300, 100, 400)] {
        body.push_str(&token_count_line(
            TokenUsage {
                input_tokens: i,
                output_tokens: o,
                cached_tokens: 0,
                cache_write_tokens: 0,
                total_tokens: t,
            },
            None,
        ));
        body.push('\n');
    }
    std::fs::write(day_dir.join(format!("{sid}.jsonl")), body).unwrap();

    let sessions = list_sessions(dir.path()).unwrap();
    assert_eq!(sessions.len(), 1);
    let s = &sessions[0];
    assert_eq!(s.input_tokens, 1500, "1000+200+300");
    assert_eq!(s.output_tokens, 350, "200+50+100");
    assert_eq!(s.total_tokens, 1850, "1200+250+400");
    assert_eq!(s.cost_usd, None, "no cost_usd on any record → None");
}

/// v1.x:每条 `TokenCount` 带 `cost_usd` → `SessionInfo.cost_usd` 为求和。
#[test]
fn list_sessions_sums_cost_usd_across_turns() {
    let dir = tempdir().unwrap();
    let day_dir = dir.path().join("2026/07/31");
    std::fs::create_dir_all(&day_dir).unwrap();
    let sid = ThreadId::new();

    let mut body = String::new();
    body.push_str(
        &serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: sid,
            model: "openai/gpt-4o".into(),
            started_at: chrono::Utc::now(),
        })
        .unwrap(),
    );
    body.push('\n');
    for c in [0.01f64, 0.008, 0.0054] {
        body.push_str(&token_count_line(
            TokenUsage {
                input_tokens: 100,
                output_tokens: 20,
                cached_tokens: 0,
                cache_write_tokens: 0,
                total_tokens: 120,
            },
            Some(c),
        ));
        body.push('\n');
    }
    std::fs::write(day_dir.join(format!("{sid}.jsonl")), body).unwrap();

    let sessions = list_sessions(dir.path()).unwrap();
    let cost = sessions[0].cost_usd.expect("cost should be present");
    // 0.01 + 0.008 + 0.0054 = 0.0234;f64 精度比较用 approx epsilon。
    assert!((cost - 0.0234).abs() < 1e-9, "cost sum mismatch: {cost}");
}

/// v1.x:旧 session(jsonl 无任何 TokenCount 记录)→ 4 个新字段为 0 / None,
/// 向后兼容。
#[test]
fn list_sessions_token_count_zero_for_old_rollouts() {
    let dir = tempdir().unwrap();
    let day_dir = dir.path().join("2026/07/31");
    std::fs::create_dir_all(&day_dir).unwrap();
    let sid = ThreadId::new();

    let mut body = String::new();
    body.push_str(
        &serde_json::to_string(&RolloutRecord::SessionMeta {
            session_id: sid,
            model: "m".into(),
            started_at: chrono::Utc::now(),
        })
        .unwrap(),
    );
    body.push('\n');
    // 只写 Message 记录(无 TokenCount),模拟 v1.x 之前的 session。
    body.push_str(
        &serde_json::to_string(&RolloutRecord::message(
            TurnId::new(),
            MessageRole::User,
            serde_json::json!("hello"),
        ))
        .unwrap(),
    );
    body.push('\n');
    std::fs::write(day_dir.join(format!("{sid}.jsonl")), body).unwrap();

    let sessions = list_sessions(dir.path()).unwrap();
    let s = &sessions[0];
    assert_eq!(s.input_tokens, 0);
    assert_eq!(s.output_tokens, 0);
    assert_eq!(s.total_tokens, 0);
    assert_eq!(s.cost_usd, None);
}
