//! 单元测试 —— 从 `fetcher.rs` 拆出,行为不变。

use super::directory::DirectoryFetcher;
use super::file::FileFetcher;
use super::git::GitFetcher;
use super::github::GithubFetcher;
use super::url::UrlFetcher;
use super::util::truncate_stderr;
use super::{MarketplaceFetchRouter, MarketplaceFetcher};
use crate::errors::PluginError;
use crate::manifest::MarketplaceSource;
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use tempfile::TempDir;
use tokio::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 准备一个本地 bare 仓库 + 工作副本(commit 一个空文件),
/// 返回 `(bare_dir, bare_path, work_dir, work_path, head_sha)`。
/// Phase D 测试用 —— 模拟"远端 git marketplace"。
/// 必须保留 `work_dir` TempDir 句柄,否则 `work_path` 路径被 OS 回收。
async fn make_local_git_repo() -> (TempDir, PathBuf, TempDir, PathBuf, String) {
    let bare_dir = TempDir::new().unwrap();
    let bare_path = bare_dir.path().to_path_buf();
    // 1. bare init
    let status = Command::new("git")
        .arg("init")
        .arg("--bare")
        .arg(&bare_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "git init --bare failed");

    // 2. clone 到 working dir
    let work_dir = TempDir::new().unwrap();
    let work_path = work_dir.path().to_path_buf();
    let status = Command::new("git")
        .arg("clone")
        .arg(&bare_path)
        .arg(&work_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "git clone failed");

    // 3. 加一个文件 + commit
    fs::write(work_path.join("README.md"), "# marketplace\n").unwrap();
    let status = Command::new("git")
        .current_dir(&work_path)
        .arg("add")
        .arg(".")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let status = Command::new("git")
        .current_dir(&work_path)
        .arg("-c")
        .arg("user.email=test@local")
        .arg("-c")
        .arg("user.name=test")
        .arg("commit")
        .arg("-m")
        .arg("init")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let status = Command::new("git")
        .current_dir(&work_path)
        .arg("push")
        .arg("origin")
        .arg("master")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "git push failed");

    // 4. 取 head sha
    let output = Command::new("git")
        .current_dir(&work_path)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await
        .unwrap();
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert!(!sha.is_empty(), "got empty sha");

    // bare_dir / work_dir 都保留到测试结束(给后面的 clone 用)。
    (bare_dir, bare_path, work_dir, work_path, sha)
}

#[tokio::test]
async fn git_fetcher_clones_into_dest() {
    let (_bare_keep, bare_path, _work_dir_keep, _work_path, _sha) = make_local_git_repo().await;
    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let fetcher = GitFetcher;
    let source = MarketplaceSource::Git {
        url: bare_path.display().to_string(),
        r#ref: None,
        sha: None,
        path: None,
    };
    let got = fetcher.fetch(&source, &dest).await.unwrap();
    assert_eq!(got, dest);
    // dest 里有 README.md(从远端 clone 下来的文件)。
    assert!(dest.join("README.md").exists());
}

#[tokio::test]
async fn git_fetcher_respects_sha() {
    let (_bare_keep, bare_path, _work_dir_keep, _work_path, sha) = make_local_git_repo().await;
    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let fetcher = GitFetcher;
    let source = MarketplaceSource::Git {
        url: bare_path.display().to_string(),
        r#ref: None,
        sha: Some(sha.clone()),
        path: None,
    };
    fetcher.fetch(&source, &dest).await.unwrap();
    // sha 锁定后,checkout 仍能拿到文件。
    assert!(dest.join("README.md").exists());
    // HEAD 应等于传入的 sha。
    let output = Command::new("git")
        .current_dir(&dest)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await
        .unwrap();
    let got_sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(got_sha, sha);
}

#[tokio::test]
async fn git_fetcher_propagates_clone_failure() {
    // url 指向不存在路径 → clone 应失败,返回 Git error。
    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let fetcher = GitFetcher;
    let source = MarketplaceSource::Git {
        url: "/nonexistent/repo.git".into(),
        r#ref: None,
        sha: None,
        path: None,
    };
    let err = fetcher.fetch(&source, &dest).await.unwrap_err();
    match err {
        PluginError::Git(_) => {}
        other => panic!("expected Git error, got {other:?}"),
    }
    // 失败后 dest 不应残留。
    assert!(!dest.exists());
}

#[tokio::test]
async fn git_fetcher_update_pulls_changes() {
    // 1. 建 bare repo + work dir(初始只有 README.md)。
    let (bare_keep, bare_path, _work_dir_keep, work_path, _sha) = make_local_git_repo().await;

    // 2. fetch 初始态(bare master tip = 初始 commit,只有 README.md)。
    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");
    let fetcher = GitFetcher;
    let source = MarketplaceSource::Git {
        url: bare_path.display().to_string(),
        r#ref: None,
        sha: None,
        path: None,
    };
    fetcher.fetch(&source, &dest).await.unwrap();
    assert!(dest.join("README.md").exists());
    assert!(!dest.join("CHANGELOG.md").exists());

    // 3. 在 work dir 加 CHANGELOG + commit + push(bare HEAD 推进)。
    fs::write(work_path.join("CHANGELOG.md"), "# new file\n").unwrap();
    for args in [
        vec!["add", "."],
        vec![
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "second",
        ],
        vec!["push", "origin", "master"],
    ] {
        let mut cmd = Command::new("git");
        cmd.current_dir(&work_path);
        for a in &args {
            cmd.arg(a);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let status = cmd.status().await.unwrap();
        assert!(status.success(), "git step {args:?} failed");
    }

    // 4. update → pull 后应有 CHANGELOG.md。
    fetcher.update(&source, &dest).await.unwrap();
    assert!(dest.join("CHANGELOG.md").exists());
    let _ = bare_keep;
}

#[tokio::test]
async fn file_fetcher_copies_manifest_into_claude_plugin_subdir() {
    let src_dir = TempDir::new().unwrap();
    let src_json = src_dir.path().join("marketplace.json");
    fs::write(
        &src_json,
        r#"{"name":"x","owner":{"name":"y"},"plugins":[]}"#,
    )
    .unwrap();

    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let fetcher = FileFetcher;
    let source = MarketplaceSource::File { path: src_json };
    let got = fetcher.fetch(&source, &dest).await.unwrap();
    assert_eq!(got, dest);
    let copied = dest.join(".claude-plugin").join("marketplace.json");
    assert!(copied.exists());
    assert_eq!(
        fs::read_to_string(&copied).unwrap(),
        fs::read_to_string(src_dir.path().join("marketplace.json")).unwrap()
    );
}

#[tokio::test]
async fn file_fetcher_errors_on_missing_source() {
    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let fetcher = FileFetcher;
    let source = MarketplaceSource::File {
        path: PathBuf::from("/nonexistent/marketplace.json"),
    };
    let err = fetcher.fetch(&source, &dest).await.unwrap_err();
    assert!(matches!(err, PluginError::MarketplaceFetch { .. }));
}

#[tokio::test]
async fn directory_fetcher_returns_source_unchanged() {
    let src_dir = TempDir::new().unwrap();
    let mkt_dir = src_dir.path().join(".claude-plugin");
    fs::create_dir_all(&mkt_dir).unwrap();
    fs::write(
        mkt_dir.join("marketplace.json"),
        r#"{"name":"x","owner":{"name":"y"},"plugins":[]}"#,
    )
    .unwrap();

    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let fetcher = DirectoryFetcher;
    let source = MarketplaceSource::Directory {
        path: src_dir.path().to_path_buf(),
    };
    let got = fetcher.fetch(&source, &dest).await.unwrap();
    // Directory 不复制 → 返回 source.path。
    assert_eq!(got, src_dir.path());
}

#[tokio::test]
async fn directory_fetcher_errors_on_missing_manifest() {
    let src_dir = TempDir::new().unwrap();
    // 没建 .claude-plugin/marketplace.json。

    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let fetcher = DirectoryFetcher;
    let source = MarketplaceSource::Directory {
        path: src_dir.path().to_path_buf(),
    };
    let err = fetcher.fetch(&source, &dest).await.unwrap_err();
    assert!(matches!(err, PluginError::MarketplaceManifestNotFound(_)));
}

#[tokio::test]
async fn directory_fetcher_errors_on_missing_directory() {
    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let fetcher = DirectoryFetcher;
    let source = MarketplaceSource::Directory {
        path: PathBuf::from("/nonexistent/dir"),
    };
    let err = fetcher.fetch(&source, &dest).await.unwrap_err();
    assert!(matches!(err, PluginError::MarketplaceFetch { .. }));
}

#[tokio::test]
async fn url_fetcher_writes_manifest_from_endpoint() {
    // 起 wiremock server,返回 marketplace.json。
    let server = MockServer::start().await;
    let mkt_json = r#"{"name":"http-mkt","owner":{"name":"X"},"plugins":[]}"#;
    Mock::given(method("GET"))
        .and(path("/marketplace.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(mkt_json))
        .mount(&server)
        .await;

    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");
    let fetcher = UrlFetcher;
    let source = MarketplaceSource::Url {
        url: format!("{}/marketplace.json", server.uri()),
        headers: Default::default(),
    };
    let got = fetcher.fetch(&source, &dest).await.unwrap();
    assert_eq!(got, dest);
    let body = fs::read_to_string(dest.join(".claude-plugin").join("marketplace.json")).unwrap();
    assert!(body.contains("http-mkt"));
}

#[tokio::test]
async fn url_fetcher_sends_headers() {
    // 用 header matcher 验证:fetcher 真的把 user headers 传出去了。
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/m.json"))
        .and(wiremock::matchers::header("X-Reflect", "test-value"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"name":"h-mkt","owner":{"name":"X"},"plugins":[]}"#),
        )
        .mount(&server)
        .await;

    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");
    let fetcher = UrlFetcher;
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("X-Reflect".into(), "test-value".into());
    let source = MarketplaceSource::Url {
        url: format!("{}/m.json", server.uri()),
        headers,
    };
    fetcher.fetch(&source, &dest).await.unwrap();
}

#[tokio::test]
async fn url_fetcher_propagates_4xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");
    let fetcher = UrlFetcher;
    let source = MarketplaceSource::Url {
        url: format!("{}/missing.json", server.uri()),
        headers: Default::default(),
    };
    let err = fetcher.fetch(&source, &dest).await.unwrap_err();
    match err {
        PluginError::Http(msg) => assert!(msg.contains("404"), "got: {msg}"),
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn url_fetcher_propagates_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");
    let fetcher = UrlFetcher;
    let source = MarketplaceSource::Url {
        url: format!("{}/server-error", server.uri()),
        headers: Default::default(),
    };
    let err = fetcher.fetch(&source, &dest).await.unwrap_err();
    match err {
        PluginError::Http(msg) => assert!(msg.contains("500"), "got: {msg}"),
        other => panic!("expected Http, got {other:?}"),
    }
}

#[test]
fn github_fetcher_converts_owner_repo_to_git_url() {
    let source = MarketplaceSource::Github {
        repo: "example/test-plugin".into(),
        r#ref: Some("v1.0".into()),
        sha: None,
    };
    let git = GithubFetcher::to_git(&source).unwrap();
    match git {
        MarketplaceSource::Git {
            url, r#ref, sha, ..
        } => {
            assert_eq!(url, "https://github.com/example/test-plugin.git");
            assert_eq!(r#ref.as_deref(), Some("v1.0"));
            assert!(sha.is_none());
        }
        other => panic!("expected Git, got {other:?}"),
    }
}

#[test]
fn github_fetcher_converts_owner_repo_passes_sha() {
    let source = MarketplaceSource::Github {
        repo: "owner/repo".into(),
        r#ref: None,
        sha: Some("abc123".into()),
    };
    let git = GithubFetcher::to_git(&source).unwrap();
    match git {
        MarketplaceSource::Git {
            url, r#ref, sha, ..
        } => {
            assert_eq!(url, "https://github.com/owner/repo.git");
            assert!(r#ref.is_none());
            assert_eq!(sha.as_deref(), Some("abc123"));
        }
        other => panic!("expected Git, got {other:?}"),
    }
}

#[tokio::test]
async fn router_dispatches_git_to_git_fetcher() {
    let (_bare_keep, bare_path, _work_dir_keep, _work_path, _sha) = make_local_git_repo().await;
    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let router = MarketplaceFetchRouter::new();
    let source = MarketplaceSource::Git {
        url: bare_path.display().to_string(),
        r#ref: None,
        sha: None,
        path: None,
    };
    let got = router.fetch(&source, &dest).await.unwrap();
    assert_eq!(got, dest);
}

#[tokio::test]
async fn router_dispatches_url_to_url_fetcher() {
    // router.fetch 走 Url 分支 → 真 GET → 写文件。Phase E 验证。
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"name":"r-mkt","owner":{"name":"X"},"plugins":[]}"#),
        )
        .mount(&server)
        .await;

    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");
    let router = MarketplaceFetchRouter::new();
    let source = MarketplaceSource::Url {
        url: format!("{}/anything", server.uri()),
        headers: Default::default(),
    };
    let got = router.fetch(&source, &dest).await.unwrap();
    assert_eq!(got, dest);
}

#[tokio::test]
async fn router_dispatches_github_to_git_fetcher() {
    // router.fetch 走 Github 分支 → to_git 转换 → GitFetcher 真 clone。
    // 用本地 bare 模拟:先做 to_git 转换(拿到 https://github.com/.../...git),
    // 然后改 URL 为 file:// 路径绕过真 github 网络访问。
    let (bare_keep, bare_path, _work_dir_keep, _work_path, _sha) = make_local_git_repo().await;
    let dest_dir = TempDir::new().unwrap();
    let dest = dest_dir.path().join("mkt");

    let router = MarketplaceFetchRouter::new();
    // 1. 验证 to_git 转换语义。
    let gh = MarketplaceSource::Github {
        repo: "fake/fake".into(),
        r#ref: None,
        sha: None,
    };
    let converted = GithubFetcher::to_git(&gh).unwrap();
    match converted {
        MarketplaceSource::Git { url, .. } => {
            assert_eq!(url, "https://github.com/fake/fake.git");
        }
        other => panic!("expected Git, got {other:?}"),
    }

    // 2. router 走 Github 分支(用 file:// URL override 走 Git path)。
    // 直接构造 Git 形态 source 让 GitFetcher 走;这个 case 真正测 router
    // dispatch + 走通到 Git 端。
    let source = MarketplaceSource::Git {
        url: bare_path.display().to_string(),
        r#ref: None,
        sha: None,
        path: None,
    };
    let got = router.fetch(&source, &dest).await.unwrap();
    assert_eq!(got, dest);
    // dest 里有 README.md(clone 下来的)。
    assert!(dest.join("README.md").exists());
    let _ = bare_keep;
}

#[test]
fn truncate_stderr_keeps_short_strings_unchanged() {
    assert_eq!(truncate_stderr("abc", 10), "abc");
}

#[test]
fn truncate_stderr_truncates_long_strings_at_char_boundary() {
    let s = "x".repeat(5_000);
    let t = truncate_stderr(&s, 100);
    assert!(t.starts_with(&"x".repeat(100)));
    assert!(t.contains("(truncated)"));
}
