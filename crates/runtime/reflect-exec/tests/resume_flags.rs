//! v0.4 Phase 2.3: 验证 `ExecArgs` 的 `-c` / `-r` / `--resume` 旗标被 clap 正确解析,
//! 并验证 `resolve_session_index` 的边界条件。
//!
//! 这些测试只跑解析层(不启动 LLM client),确保 conflict 校验按预期工作。

use clap::Parser;
use reflect_exec::ExecArgs;
use reflect_rollout::index::resolve_session_index;
use std::path::Path;
use tempfile::tempdir;

/// `reflect exec` CLI 命令结构,ExecArgs flatten 进 args。
#[derive(Parser, Debug)]
struct Wrap {
    #[command(subcommand)]
    cmd: WrapCmd,
}

#[derive(Parser, Debug)]
enum WrapCmd {
    Exec {
        #[command(flatten)]
        args: ExecArgs,
    },
}

/// 从 `WrapCmd::Exec` 拿 args(单 variant,直接 destructure 避免 irrefutable warning)。
fn args_of(w: Wrap) -> ExecArgs {
    match w.cmd {
        WrapCmd::Exec { args } => args,
    }
}

/// `-c` 单独使用:clap 接受,`continue_last == true`。
#[test]
fn exec_args_continue_last_parses() {
    let w = Wrap::parse_from(["reflect", "exec", "--continue-last"]);
    let args = args_of(w);
    assert!(args.continue_last);
    assert!(args.resume_by.is_none());
    assert!(args.resume.is_none());
    assert!(args.prompt.is_none());
}

/// `-r 3` 单独使用:clap 接受,`resume_by == Some(3)`。
#[test]
fn exec_args_resume_by_parses() {
    let w = Wrap::parse_from(["reflect", "exec", "-r", "3"]);
    let args = args_of(w);
    assert!(!args.continue_last);
    assert_eq!(args.resume_by, Some(3));
    assert!(args.resume.is_none());
    assert!(args.prompt.is_none());
}

/// `--resume <uuid>` 单独使用:clap 接受,`resume == Some(uuid)`。
#[test]
fn exec_args_resume_uuid_parses() {
    let w = Wrap::parse_from([
        "reflect",
        "exec",
        "--resume",
        "11111111-2222-3333-4444-555555555555",
    ]);
    let args = args_of(w);
    assert!(!args.continue_last);
    assert!(args.resume_by.is_none());
    assert_eq!(
        args.resume.as_deref(),
        Some("11111111-2222-3333-4444-555555555555")
    );
    assert!(args.prompt.is_none());
}

/// `-c` + `-r` → clap reject(`multiple = false` group)。
#[test]
fn exec_args_continue_last_and_resume_by_conflict() {
    let r = Wrap::try_parse_from(["reflect", "exec", "-c", "-r", "1"]);
    assert!(r.is_err(), "expected clap to reject -c + -r conflict");
}

/// `-c` + `--resume` → clap reject。
#[test]
fn exec_args_continue_last_and_resume_conflict() {
    let r = Wrap::try_parse_from([
        "reflect",
        "exec",
        "-c",
        "--resume",
        "11111111-2222-3333-4444-555555555555",
    ]);
    assert!(r.is_err(), "expected clap to reject -c + --resume conflict");
}

/// `-r 1` + `--resume <uuid>` → clap reject。
#[test]
fn exec_args_resume_by_and_resume_conflict() {
    let r = Wrap::try_parse_from([
        "reflect",
        "exec",
        "-r",
        "1",
        "--resume",
        "11111111-2222-3333-4444-555555555555",
    ]);
    assert!(r.is_err(), "expected clap to reject -r + --resume conflict");
}

/// `--prompt "hi"` + `-c` → clap reject(position prompt 在 group `input` 与 `-c` 互斥)。
#[test]
fn exec_args_prompt_and_continue_last_conflict() {
    let r = Wrap::try_parse_from(["reflect", "exec", "--prompt", "hi", "-c"]);
    assert!(r.is_err(), "expected clap to reject --prompt + -c conflict");
}

/// `--prompt "hi"` + `-r 1` → clap reject。
#[test]
fn exec_args_prompt_and_resume_by_conflict() {
    let r = Wrap::try_parse_from(["reflect", "exec", "--prompt", "hi", "-r", "1"]);
    assert!(r.is_err(), "expected clap to reject --prompt + -r conflict");
}

/// `--prompt "hi"` + `--resume <uuid>` → clap reject。
#[test]
fn exec_args_prompt_and_resume_uuid_conflict() {
    let r = Wrap::try_parse_from([
        "reflect",
        "exec",
        "--prompt",
        "hi",
        "--resume",
        "11111111-2222-3333-4444-555555555555",
    ]);
    assert!(
        r.is_err(),
        "expected clap to reject --prompt + --resume conflict"
    );
}

/// 空调用(`reflect exec`)→ clap 接受(所有字段都是 `Option`),但所有字段都是 `None`。
/// 运行时由 `async_main` 检测"四个 input 字段全 None"再报错。
#[test]
fn exec_args_no_args_all_none() {
    let w = Wrap::parse_from(["reflect", "exec"]);
    let args = args_of(w);
    assert!(!args.continue_last);
    assert!(args.resume_by.is_none());
    assert!(args.resume.is_none());
    assert!(args.prompt.is_none());
}

/// `resolve_session_index` 在 tmpdir 里两条 session 时,`-c` 选最新的。
#[test]
fn resolve_session_index_continue_last_in_tmpdir() {
    let dir = tempdir().unwrap();
    let (newest, _older) = write_two_sessions(dir.path());
    let picked = resolve_session_index(dir.path(), true, None).unwrap();
    assert_eq!(picked, newest);
}

/// `resolve_session_index` 在 tmpdir 里两条 session 时,`-r 2` 选次新的。
#[test]
fn resolve_session_index_resume_by_two_in_tmpdir() {
    let dir = tempdir().unwrap();
    let (_newest, older) = write_two_sessions(dir.path());
    let picked = resolve_session_index(dir.path(), false, Some(2)).unwrap();
    assert_eq!(picked, older);
}

/// 写两条 session 到 tmpdir,返回 (newest_session_id, older_session_id)。
fn write_two_sessions(dir: &Path) -> (reflect_protocol::ThreadId, reflect_protocol::ThreadId) {
    use chrono::{TimeZone, Utc};
    use reflect_protocol::{RolloutRecord, ThreadId};

    let newer_id = ThreadId::new();
    let older_id = ThreadId::new();
    let newer = Utc.with_ymd_and_hms(2026, 6, 18, 12, 0, 0).unwrap();
    let older = Utc.with_ymd_and_hms(2026, 6, 18, 11, 0, 0).unwrap();

    for (id, started) in [(newer_id, newer), (older_id, older)] {
        let path = reflect_rollout::path::session_path_at(dir, id, started);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string(&RolloutRecord::SessionMeta {
                session_id: id,
                model: "openai/gpt-4o".into(),
                started_at: started,
            })
            .unwrap(),
        )
        .unwrap();
    }
    (newer_id, older_id)
}
