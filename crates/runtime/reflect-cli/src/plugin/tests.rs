//! `reflect plugin ...` 单测。整体迁自原 `plugin.rs` 内联 `#[cfg(test)] mod tests`。

use super::*;
use crate::plugin::marketplace::load_known_marketplaces;
use crate::test_home::{lock_home, set_home as reclaim_home};
use std::fs;
use tempfile::TempDir;

/// 构造最小可装 plugin fixture(给 install / list / enable 路径用)。
fn make_plugin(parent: &Path, name: &str) -> PathBuf {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("plugin.toml"),
        format!("name = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"test plugin\"\n"),
    )
    .unwrap();
    dir
}

/// 单元层:truncate 行为与 mcp.rs 镜像。
#[test]
fn truncate_helper_works() {
    assert_eq!(truncate("abc", 4), "abc");
    assert_eq!(truncate("abcdefgh", 4), "abc…");
    assert_eq!(truncate("", 4), "");
}

/// 单元层:`render_install_report` 在全成功场景只打印 ✓ 块。
#[test]
fn render_install_report_success_only_prints_installed_block() {
    let mut report = reflect_plugin::InstallReport::default();
    let id = PluginId::new("demo", "smoke-mkt").unwrap();
    let entry = reflect_plugin::state::InstallationEntry {
        scope: PluginScope::User,
        install_path: std::path::PathBuf::from("/tmp/cache/smoke-mkt/demo/1.0.0"),
        version: "1.0.0".into(),
        installed_at: reflect_plugin::state_now(),
        last_updated: reflect_plugin::state_now(),
        git_commit_sha: None,
        checksum_sha256: None,
    };
    report.installed.push((id, entry));
    // 抓 stdout → 用 `print!` 重新写,验证块存在。
    // 这里只验证不 panic 且不打印 ✗ 块(`failed` 为空跳过)。
    render_install_report(&report, "smoke-mkt");
    // assert! 取代:`is_success()` 必须为 true,且 `failed` 为空。
    assert!(report.is_success());
    assert!(report.failed.is_empty());
}

/// 单元层:`render_install_report` 混合场景:成功 + 失败都打印。
#[test]
fn render_install_report_mixed_prints_both_blocks() {
    let mut report = reflect_plugin::InstallReport::default();
    let id = PluginId::new("demo", "smoke-mkt").unwrap();
    let entry = reflect_plugin::state::InstallationEntry {
        scope: PluginScope::User,
        install_path: std::path::PathBuf::from("/tmp/cache/smoke-mkt/demo/1.0.0"),
        version: "1.0.0".into(),
        installed_at: reflect_plugin::state_now(),
        last_updated: reflect_plugin::state_now(),
        git_commit_sha: None,
        checksum_sha256: None,
    };
    report.installed.push((id, entry));
    report.failed.push(reflect_plugin::PluginInstallFailure {
        target: "ghost@smoke-mkt".to_string(),
        error: reflect_plugin::PluginError::NotInstalled("ghost".into()),
    });
    render_install_report(&report, "smoke-mkt");
    assert!(!report.is_success());
    assert_eq!(report.installed.len(), 1);
    assert_eq!(report.failed.len(), 1);
}

/// 单元层:scope label 不变。
#[test]
fn scope_label_maps_all_variants() {
    assert_eq!(scope_label(PluginScope::Managed), "managed");
    assert_eq!(scope_label(PluginScope::User), "user");
    assert_eq!(scope_label(PluginScope::Project), "project");
    assert_eq!(scope_label(PluginScope::Local), "local");
}

/// 单元层:parse_id 接受标准 plugin id。
#[test]
fn parse_id_accepts_standard_id() {
    let id = parse_id("demo@inline").unwrap();
    assert_eq!(id.as_str(), "demo@inline");
}

/// 单元层:parse_id 拒绝非法 id。
#[test]
fn parse_id_rejects_invalid() {
    let err = parse_id("Bad Name@inline").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("invalid plugin id"), "got: {msg}");
}

/// 端到端:install → list → enable → list(显示 enabled)→ disable → uninstall。
/// 走真 IO(用 HOME 重定向到 tmpdir)。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn install_enable_disable_uninstall_round_trip() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());

    // 准备 plugin fixture(make_plugin 已用单一 tab 缩进,直接用即可)。
    let src = make_plugin(&home.path().join("src"), "demo");

    // 1. 安装插件
    install(&src, PluginScope::User).await.unwrap();
    let (mgr, _cfg) = load_manager_and_config().unwrap();
    let id = PluginId::inline("demo").unwrap();
    assert!(mgr.lookup_entry(&id).is_some(), "installed entry missing");

    // 2. list → 显示 installed,1 条
    list().unwrap();
    // list 只 print,不返回值;从 state 读。
    let enabled_empty: HashSet<String> = HashSet::new();
    let rows = mgr.list_with_enabled(&enabled_empty);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, id);

    // 3. 启用插件
    enable("demo@inline").unwrap();
    let cfg = load_default();
    assert!(cfg.plugins.is_enabled("demo@inline"));

    // 4. 列表应显示 enabled=true
    let enabled_set: HashSet<String> = cfg.plugins.enabled_plugins.iter().cloned().collect();
    let rows2 = mgr.list_with_enabled(&enabled_set);
    assert!(rows2.iter().any(|(pid, _, on)| pid == &id && *on));

    // 5. 停用插件
    disable("demo@inline").unwrap();
    let cfg = load_default();
    assert!(!cfg.plugins.is_enabled("demo@inline"));

    // 6. uninstall(yes 跳过提示)
    uninstall("demo@inline", PluginScope::User, true).unwrap();
    let (mgr2, cfg2) = load_manager_and_config().unwrap();
    assert!(mgr2.lookup_entry(&id).is_none());
    assert!(!cfg2.plugins.is_enabled("demo@inline"));
}

/// enable 未知 plugin 报错。
#[test]
fn enable_unknown_plugin_errors() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let err = enable("ghost@inline").unwrap_err();
    assert!(format!("{err}").contains("not installed"));
}

/// disable 不在 enabled 列表的 plugin 不报错(no-op 文案)。
#[test]
fn disable_not_enabled_is_noop() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    // 没 install 直接 disable → 应 no-op(不报错)。
    disable("ghost@inline").unwrap();
}

/// uninstall 未知 plugin 报错。
#[test]
fn uninstall_unknown_plugin_errors() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let err = uninstall("ghost@inline", PluginScope::User, true).unwrap_err();
    assert!(format!("{err}").contains("uninstall failed"));
}

/// info 对未装 plugin 报错。
#[test]
fn info_unknown_plugin_errors() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let err = info("ghost@inline").unwrap_err();
    assert!(format!("{err}").contains("not installed"));
}

/// info 对已装 plugin 返回完整文本(只需不 panic)。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn info_installed_plugin_does_not_panic() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let src = make_plugin(&home.path().join("src"), "demo");
    install(&src, PluginScope::User).await.unwrap();
    info("demo@inline").unwrap();
}

/// show_manifest 找不到 manifest 时报错。
#[test]
fn show_missing_manifest_errors() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    // 直接构造一份 installed_plugins.json 指向空目录。
    let plugins_root = default_plugins_root().unwrap();
    fs::create_dir_all(&plugins_root).unwrap();
    let mgr_path = plugins_root.join("installed_plugins.json");
    let id = PluginId::inline("ghost").unwrap();
    let entry = reflect_plugin::state::InstallationEntry {
        scope: PluginScope::User,
        install_path: home.path().join("nowhere"),
        version: "1.0.0".into(),
        installed_at: reflect_plugin::state_now(),
        last_updated: reflect_plugin::state_now(),
        git_commit_sha: None,
        checksum_sha256: None,
    };
    let mut f = InstalledPluginsFile::new();
    f.upsert(id.clone(), entry);
    fs::write(&mgr_path, serde_json::to_string_pretty(&f).unwrap()).unwrap();

    let err = show_manifest("ghost@inline").unwrap_err();
    assert!(format!("{err}").contains("manifest missing"));
}

/// show_manifest 找到 manifest 时打印其内容。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn show_manifest_prints_content() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let src = make_plugin(&home.path().join("src"), "demo");
    install(&src, PluginScope::User).await.unwrap();
    show_manifest("demo@inline").unwrap();
}

/// marketplace_ls 空表不 panic。
#[test]
fn marketplace_ls_empty_does_not_panic() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    marketplace_ls().unwrap();
}

/// marketplace_add directory 真接通 —— 准备一个本地 marketplace 根目录,
/// 调 marketplace_add --from directory --path <root>,验证 known_marketplaces.json
/// 写入且 .claude-plugin/marketplace.json 被 find_in_dir 找到。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_add_directory_writes_known_marketplaces() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());

    // 1. 准备 marketplace fixture
    let mkt_root = home.path().join("local-mkt");
    fs::create_dir_all(mkt_root.join(".claude-plugin")).unwrap();
    let plugin_dir = mkt_root.join("plugins").join("demo");
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(
        plugin_dir.join("plugin.toml"),
        "name = \"demo\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    fs::write(
        mkt_root.join(".claude-plugin").join("marketplace.json"),
        r#"{
                "name": "local-mkt",
                "owner": { "name": "Local" },
                "plugins": [
                    {
                        "name": "demo",
                        "version": "1.0.0",
                        "source": { "source": "directory", "path": "./plugins/demo" }
                    }
                ]
            }"#,
    )
    .unwrap();

    // 2. marketplace_add 真接通
    marketplace_add(
        "local-mkt",
        crate::MarketplaceSourceKind::Directory,
        None,
        None,
        None,
        None,
        Some(mkt_root.to_str().unwrap()),
    )
    .await
    .unwrap();

    // 3. 验证 known_marketplaces.json
    let mp_path = default_marketplaces_path().unwrap();
    let f = load_known_marketplaces(&mp_path).unwrap();
    assert!(
        f.marketplaces
            .contains_key(&reflect_plugin::MarketplaceName::parse("local-mkt").unwrap())
    );
}

/// marketplace_add file 真接通 —— 把单文件 marketplace.json 复制到 cache。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_add_file_copies_into_cache_subdir() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());

    let src_json = home.path().join("marketplace.json");
    fs::write(
        &src_json,
        r#"{"name":"file-mkt","owner":{"name":"X"},"plugins":[]}"#,
    )
    .unwrap();

    marketplace_add(
        "file-mkt",
        crate::MarketplaceSourceKind::File,
        None,
        None,
        None,
        None,
        Some(src_json.to_str().unwrap()),
    )
    .await
    .unwrap();

    // cache 应有 .claude-plugin/marketplace.json
    let cache_dir = home
        .path()
        .join(".reflect")
        .join("plugins")
        .join("marketplaces")
        .join("file-mkt");
    assert!(
        cache_dir
            .join(".claude-plugin")
            .join("marketplace.json")
            .exists()
    );
}

/// marketplace_add github 缺 --repo 报错(Phase E 起真的接,先校验 flag)。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_add_github_requires_repo() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let err = marketplace_add(
        "x",
        crate::MarketplaceSourceKind::Github,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("--repo"));
}

/// marketplace_add url 缺 --url 报错。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_add_url_requires_url_flag() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let err = marketplace_add(
        "x",
        crate::MarketplaceSourceKind::Url,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("--url"));
}

/// marketplace_add url 接 wiremock 本地 server,真 GET → 写 known_marketplaces.json。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_add_url_works_against_local_mock() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/m.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"name":"http-mkt","owner":{"name":"X"},"plugins":[]}"#),
        )
        .mount(&server)
        .await;

    marketplace_add(
        "http-mkt",
        crate::MarketplaceSourceKind::Url,
        None,
        Some(&format!("{}/m.json", server.uri())),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // cache 应有 .claude-plugin/marketplace.json
    let cache_dir = home
        .path()
        .join(".reflect")
        .join("plugins")
        .join("marketplaces")
        .join("http-mkt");
    assert!(
        cache_dir
            .join(".claude-plugin")
            .join("marketplace.json")
            .exists()
    );
}

/// marketplace_refresh 不带 --name → 走 refresh_all。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_refresh_all_no_markets_noop() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    // 无 known_marketplaces.json → 走 "(no marketplaces configured)"。
    marketplace_refresh(None).await.unwrap();
}

/// marketplace_refresh 指定不存在的 name → 报错。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_refresh_unknown_name_errors() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let err = marketplace_refresh(Some("ghost")).await.unwrap_err();
    assert!(format!("{err}").contains("not registered"));
}

/// marketplace_refresh 已知 directory marketplace → OK。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_refresh_known_directory_succeeds() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());

    // 1. marketplace_add directory(让 known_marketplaces 写入)。
    let mkt_root = home.path().join("d-mkt");
    fs::create_dir_all(mkt_root.join(".claude-plugin")).unwrap();
    fs::write(
        mkt_root.join(".claude-plugin").join("marketplace.json"),
        r#"{"name":"d-mkt","owner":{"name":"X"},"plugins":[]}"#,
    )
    .unwrap();
    marketplace_add(
        "d-mkt",
        crate::MarketplaceSourceKind::Directory,
        None,
        None,
        None,
        None,
        Some(mkt_root.to_str().unwrap()),
    )
    .await
    .unwrap();

    // 2. marketplace_refresh 应 OK(directory no-op)。
    marketplace_refresh(Some("d-mkt")).await.unwrap();
}

/// marketplace_add git 但缺 --url 报错。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_add_git_requires_url() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let err = marketplace_add(
        "x",
        crate::MarketplaceSourceKind::Git,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("--url"));
}

/// marketplace_add directory 但缺 --path 报错。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_add_directory_requires_path() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let err = marketplace_add(
        "x",
        crate::MarketplaceSourceKind::Directory,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("--path"));
}

/// marketplace_add 非法名字(inline)拒绝。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_add_rejects_reserved_name() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let mkt_root = home.path().join("dummy");
    fs::create_dir_all(mkt_root.join(".claude-plugin")).unwrap();
    fs::write(
        mkt_root.join(".claude-plugin").join("marketplace.json"),
        "{}",
    )
    .unwrap();
    let err = marketplace_add(
        "inline",
        crate::MarketplaceSourceKind::Directory,
        None,
        None,
        None,
        None,
        Some(mkt_root.to_str().unwrap()),
    )
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("invalid marketplace name"));
}

/// marketplace_remove 真删 install_location。
/// Phase D 决策:Directory fetcher 不复制 → install_location 直接指向
/// source.path。remove 会清掉 source.path 整个目录。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_remove_purges_install_location() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());

    // 1. add 一个 directory marketplace —— install_location = source.path
    let mkt_root = home.path().join("purge-mkt");
    fs::create_dir_all(mkt_root.join(".claude-plugin")).unwrap();
    fs::write(
        mkt_root.join(".claude-plugin").join("marketplace.json"),
        r#"{"name":"purge-mkt","owner":{"name":"X"},"plugins":[]}"#,
    )
    .unwrap();
    marketplace_add(
        "purge-mkt",
        crate::MarketplaceSourceKind::Directory,
        None,
        None,
        None,
        None,
        Some(mkt_root.to_str().unwrap()),
    )
    .await
    .unwrap();

    // 2. source.path 应存在(Directory fetcher 不复制,直接用 source.path)。
    assert!(mkt_root.exists());
    assert!(mkt_root.join(".claude-plugin/marketplace.json").exists());

    // 3. remove —— 真删 install_location(= source.path)。
    marketplace_remove("purge-mkt").unwrap();

    // 4. install_location 应被删。
    assert!(!mkt_root.exists());
}

/// marketplace_remove 不存在的名字报错。
#[test]
fn marketplace_remove_unknown_errors() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let err = marketplace_remove("ghost").unwrap_err();
    assert!(format!("{err}").contains("not registered"));
}

/// marketplace_add git 真 clone(用本地 bare 仓库)。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn marketplace_add_git_clones_into_cache() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());

    // 1. 准备 bare 仓库 —— HOME 被重定向到空 tempdir,git 的
    //    init.defaultBranch 退回系统默认(可能 main),用 `-c init.defaultBranch=master`
    //    显式锁住 master 分支名,后续 push 不会因 main/master 不匹配失败。
    let bare_dir = home.path().join("test.git");
    let out = std::process::Command::new("git")
        .arg("-c")
        .arg("init.defaultBranch=master")
        .arg("init")
        .arg("--bare")
        .arg(&bare_dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git init --bare failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let work_dir = home.path().join("work");
    let out = std::process::Command::new("git")
        .arg("-c")
        .arg("init.defaultBranch=master")
        .arg("clone")
        .arg(&bare_dir)
        .arg(&work_dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git clone (work) failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::create_dir_all(work_dir.join(".claude-plugin")).unwrap();
    fs::write(
        work_dir.join(".claude-plugin").join("marketplace.json"),
        r#"{"name":"git-mkt","owner":{"name":"X"},"plugins":[]}"#,
    )
    .unwrap();
    let out = std::process::Command::new("git")
        .current_dir(&work_dir)
        .arg("add")
        .arg(".")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git add failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = std::process::Command::new("git")
        .current_dir(&work_dir)
        .arg("-c")
        .arg("user.email=t@t")
        .arg("-c")
        .arg("user.name=t")
        .arg("commit")
        .arg("-m")
        .arg("init")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git commit failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = std::process::Command::new("git")
        .current_dir(&work_dir)
        .arg("push")
        .arg("origin")
        .arg("master")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git push failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. marketplace_add --from git --url <bare>(裸 URL)
    marketplace_add(
        "git-mkt",
        crate::MarketplaceSourceKind::Git,
        None,
        Some(bare_dir.to_str().unwrap()),
        None,
        None,
        None,
    )
    .await
    .unwrap();

    // 3. cache 应有 .claude-plugin/marketplace.json(clone 下来的)
    reclaim_home(home.path());
    let mp_path = default_marketplaces_path().expect("HOME set");
    let known = load_known_marketplaces(&mp_path).unwrap();
    let name = reflect_plugin::MarketplaceName::parse("git-mkt").unwrap();
    let entry = known
        .marketplaces
        .get(&name)
        .expect("git-mkt should be registered");
    let manifest = entry
        .install_location
        .join(".claude-plugin")
        .join("marketplace.json");
    assert!(
        manifest.exists(),
        "manifest missing at {}",
        manifest.display()
    );
}

/// manifest 解析为空数据也能正常 install(测 plugin.toml 不带 version)。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn install_minimal_manifest_with_no_version() {
    let home = TempDir::new().unwrap();
    let _guard = lock_home(home.path());
    let src = home.path().join("src/minimal");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("plugin.toml"), "name = \"minimal\"\n").unwrap();
    install(&src, PluginScope::User).await.unwrap();
}

// ── PluginManifest 解析 sanity (跨 crate 用法) ─────────────────────

/// 确保 `PluginManifest::from_path` 能直接读 manifest(给 show / info 复用)。
#[test]
fn manifest_from_path_reads_real_plugin_toml() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("plugin.toml");
    fs::write(
        &p,
        r#"
name = "x"
version = "2.0.0"
description = "round trip"
"#,
    )
    .unwrap();
    let m = PluginManifest::from_path(&p).unwrap();
    assert_eq!(m.name, "x");
    assert_eq!(m.version.as_deref(), Some("2.0.0"));
}
