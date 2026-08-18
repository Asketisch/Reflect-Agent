//! 单元测试 —— 从 `manager.rs` 拆出,行为不变。

use super::*;
use crate::errors::PluginError;
use crate::identifier::MarketplaceName;
use crate::identifier::PluginId;
use crate::state::{InstallationEntry, KnownMarketplacesFile, PluginScope};
use chrono::Utc;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// 构造一个最小可装的 plugin 目录 fixture。
fn make_plugin_fixture(parent: &Path, name: &str, version: &str) {
    let dir = parent.join(name);
    fs::create_dir_all(&dir).unwrap();
    let manifest = format!(
        r#"
name = "{name}"
version = "{version}"
description = "test fixture"
"#
    );
    fs::write(dir.join("plugin.toml"), manifest).unwrap();
    // 加一个 sub-file,确保递归复制生效。
    fs::create_dir_all(dir.join("commands")).unwrap();
    fs::write(dir.join("commands").join("hello.md"), "# hello\n").unwrap();
}

#[test]
fn install_local_copies_plugin_into_cache() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let source_root = tmp.path().join("source");
    make_plugin_fixture(&source_root, "demo", "1.0.0");
    // fixture 在 <source_root>/demo/ 下,install 传它。
    let source_dir = source_root.join("demo");

    let mut mgr = PluginManager::new(&plugins_root);
    let (id, entry) = mgr
        .install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();

    assert_eq!(id.as_str(), "demo@inline");
    assert_eq!(entry.version, "1.0.0");
    assert_eq!(entry.scope, PluginScope::User);
    assert!(entry.install_path.exists());
    // manifest 文件复制成功。
    assert!(entry.install_path.join("plugin.toml").exists());
    // 递归子目录也复制。
    assert!(
        entry
            .install_path
            .join("commands")
            .join("hello.md")
            .exists()
    );
    // JSON 落盘。
    assert!(plugins_root.join(INSTALLED_FILE).exists());
}

#[test]
fn install_local_persists_state() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let source_root = tmp.path().join("source");
    make_plugin_fixture(&source_root, "demo", "1.0.0");
    let source_dir = source_root.join("demo");

    let mut mgr = PluginManager::new(&plugins_root);
    let (id, _) = mgr
        .install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();

    // 重新 load,验证状态持久化。
    let mgr2 = PluginManager::load(&plugins_root).unwrap();
    assert!(mgr2.state().plugins.contains_key(&id));
}

#[test]
fn install_rejects_duplicate_in_same_scope() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let source_root = tmp.path().join("source");
    make_plugin_fixture(&source_root, "demo", "1.0.0");
    let source_dir = source_root.join("demo");

    let mut mgr = PluginManager::new(&plugins_root);
    mgr.install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();
    // 同 scope 二次 install → AlreadyInstalled。
    let err = mgr
        .install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap_err();
    assert!(matches!(err, PluginError::AlreadyInstalled(_)));
}

#[test]
fn install_rejects_missing_manifest() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let empty = tmp.path().join("empty");
    fs::create_dir_all(&empty).unwrap();

    let mut mgr = PluginManager::new(&plugins_root);
    let err = mgr
        .install_local(&empty, &MarketplaceName::inline(), PluginScope::User)
        .unwrap_err();
    assert!(matches!(err, PluginError::ManifestParse { .. }));
}

#[test]
fn install_rejects_bad_manifest_name() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let bad = tmp.path().join("bad");
    fs::create_dir_all(&bad).unwrap();
    // 含空格 → 段级 `validate_segment` 命中 `Validation`。
    fs::write(
        bad.join("plugin.toml"),
        "name = \"bad name\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();

    let mut mgr = PluginManager::new(&plugins_root);
    let err = mgr
        .install_local(&bad, &MarketplaceName::inline(), PluginScope::User)
        .unwrap_err();
    assert!(
        matches!(
            err,
            PluginError::InvalidPluginId(_) | PluginError::Validation(_)
        ),
        "got {err:?}"
    );
}

#[test]
fn uninstall_managed_rejected() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    // 直接塞一个 managed 条目到 state(模拟企业 pin)。
    let id = PluginId::parse("pinned@marketplace").unwrap();
    mgr.state.plugins.insert(
        id.clone(),
        vec![InstallationEntry {
            scope: PluginScope::Managed,
            install_path: PathBuf::from("/nonexistent"),
            version: "1.0.0".into(),
            installed_at: Utc::now(),
            last_updated: Utc::now(),
            git_commit_sha: None,
            checksum_sha256: None,
        }],
    );
    let err = mgr.uninstall(&id, PluginScope::Managed).unwrap_err();
    assert!(matches!(err, PluginError::ManagedLocked(_)));
}

#[test]
fn uninstall_unknown_plugin_errors() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = PluginManager::new(tmp.path());
    let id = PluginId::inline("ghost").unwrap();
    let err = mgr.uninstall(&id, PluginScope::User).unwrap_err();
    assert!(matches!(err, PluginError::NotInstalled(_)));
}

#[test]
fn list_installed_returns_in_id_order() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    // 直接塞两个 entry。
    let a = PluginId::inline("aaa").unwrap();
    let b = PluginId::inline("bbb").unwrap();
    let entry_a = InstallationEntry {
        scope: PluginScope::User,
        install_path: PathBuf::from("/a"),
        version: "1.0.0".into(),
        installed_at: Utc::now(),
        last_updated: Utc::now(),
        git_commit_sha: None,
        checksum_sha256: None,
    };
    let entry_b = InstallationEntry {
        scope: PluginScope::User,
        install_path: PathBuf::from("/b"),
        version: "1.0.0".into(),
        installed_at: Utc::now(),
        last_updated: Utc::now(),
        git_commit_sha: None,
        checksum_sha256: None,
    };
    mgr.state.plugins.insert(b.clone(), vec![entry_b.clone()]);
    mgr.state.plugins.insert(a.clone(), vec![entry_a.clone()]);
    let out = mgr.list_installed();
    assert_eq!(out[0].0, a);
    assert_eq!(out[1].0, b);
}

#[test]
fn cache_path_matches_claude_code_layout() {
    let tmp = TempDir::new().unwrap();
    let mgr = PluginManager::new(tmp.path());
    let id = PluginId::parse("demo@anthropic-tools").unwrap();
    let p = mgr.cache_path_for(&id, "1.2.3");
    assert!(p.ends_with("cache/anthropic-tools/demo/1.2.3"));
}

#[test]
fn save_is_atomic_via_rename() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let mgr = PluginManager::new(&plugins_root);
    mgr.save().unwrap();
    // 不留 .tmp 残留。
    assert!(!plugins_root.join(format!("{INSTALLED_FILE}.tmp")).exists());
    assert!(plugins_root.join(INSTALLED_FILE).exists());
}

#[test]
fn load_returns_empty_when_file_missing() {
    let tmp = TempDir::new().unwrap();
    let mgr = PluginManager::load(tmp.path()).unwrap();
    assert!(mgr.state().plugins.is_empty());
}

#[test]
fn load_recovers_from_existing_state() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let source_root = tmp.path().join("source");
    make_plugin_fixture(&source_root, "demo", "2.0.0");
    let source_dir = source_root.join("demo");

    // 先装再 load。
    let mut mgr1 = PluginManager::new(&plugins_root);
    mgr1.install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();

    let mgr2 = PluginManager::load(&plugins_root).unwrap();
    assert_eq!(mgr2.state().plugins.len(), 1);
    let entries: Vec<&InstallationEntry> = mgr2
        .state()
        .plugins
        .values()
        .flat_map(|v| v.iter())
        .collect();
    assert_eq!(entries[0].version, "2.0.0");
}

#[test]
fn save_then_load_round_trips_complex_state() {
    // 测 BTreeMap + 多 scope 同 id 的 round-trip。
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    let id = PluginId::inline("demo").unwrap();
    mgr.state.plugins.insert(
        id.clone(),
        vec![
            InstallationEntry {
                scope: PluginScope::User,
                install_path: PathBuf::from("/user"),
                version: "1.0.0".into(),
                installed_at: Utc::now(),
                last_updated: Utc::now(),
                git_commit_sha: None,
                checksum_sha256: None,
            },
            InstallationEntry {
                scope: PluginScope::Local,
                install_path: PathBuf::from("/local"),
                version: "1.1.0".into(),
                installed_at: Utc::now(),
                last_updated: Utc::now(),
                git_commit_sha: None,
                checksum_sha256: None,
            },
        ],
    );
    mgr.save().unwrap();
    let mgr2 = PluginManager::load(&plugins_root).unwrap();
    let entries = mgr2.state.plugins.get(&id).unwrap();
    assert_eq!(entries.len(), 2);
    // entries_for 按 scope precedence 降序:Local > User。
    let sorted = mgr2.entries_for(&id);
    assert_eq!(sorted[0].scope, PluginScope::Local);
    assert_eq!(sorted[1].scope, PluginScope::User);
}

#[test]
fn manifest_name_normalizes_case_on_install() {
    // PluginId::inline 内部 lower-case 化,所以 "Demo" 应变成 "demo@inline"。
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let source_dir = tmp.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(
        source_dir.join("plugin.toml"),
        "name = \"Demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let mut mgr = PluginManager::new(&plugins_root);
    let (id, _) = mgr
        .install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();
    assert_eq!(id.as_str(), "demo@inline");
    // cache 路径用归一化的 id.marketplace() / id.name()。
    let p = mgr.cache_path_for(&id, "0.1.0");
    assert!(p.ends_with("cache/inline/demo/0.1.0"));
}

#[test]
fn install_with_default_version_uses_0_0_0() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let source_dir = tmp.path().join("source");
    fs::create_dir_all(&source_dir).unwrap();
    fs::write(source_dir.join("plugin.toml"), "name = \"demo\"\n").unwrap();
    let mut mgr = PluginManager::new(&plugins_root);
    let (_id, entry) = mgr
        .install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();
    assert_eq!(entry.version, "0.0.0");
}

// ── v1.0.0-rc2:生命周期 helpers ──────────────────────────────────

#[test]
fn touch_updates_last_updated_and_persists() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let source_dir = tmp.path().join("source");
    make_plugin_fixture(&source_dir, "demo", "1.0.0");
    let source_dir = source_dir.join("demo");

    let mut mgr = PluginManager::new(&plugins_root);
    let (id, entry_before) = mgr
        .install_local(&source_dir, &MarketplaceName::inline(), PluginScope::User)
        .unwrap();
    // 短 sleep 保证 Utc::now() 至少推进 1ns;实际 fixture 装得很慢,
    // 但 UTC 精度足够,这里直接对比 "before < after" 即可。
    std::thread::sleep(std::time::Duration::from_millis(5));
    mgr.touch(&id, PluginScope::User).unwrap();

    // 落盘后 reload,验证 last_updated 已更新。
    let mgr2 = PluginManager::load(&plugins_root).unwrap();
    let after = mgr2.lookup_entry(&id).unwrap();
    assert!(
        after.last_updated > entry_before.last_updated,
        "touch should bump last_updated"
    );
}

#[test]
fn touch_unknown_plugin_errors() {
    let tmp = TempDir::new().unwrap();
    let mut mgr = PluginManager::new(tmp.path());
    let id = PluginId::inline("ghost").unwrap();
    let err = mgr.touch(&id, PluginScope::User).unwrap_err();
    assert!(matches!(err, PluginError::NotInstalled(_)));
}

#[test]
fn list_with_enabled_marks_correctly() {
    let tmp = TempDir::new().unwrap();
    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    // 直接塞三个 entry,模拟已装。
    let id_a = PluginId::inline("aaa").unwrap();
    let id_b = PluginId::inline("bbb").unwrap();
    let id_c = PluginId::inline("ccc").unwrap();
    let mk = |scope, path: &str| InstallationEntry {
        scope,
        install_path: PathBuf::from(path),
        version: "1.0.0".into(),
        installed_at: Utc::now(),
        last_updated: Utc::now(),
        git_commit_sha: None,
        checksum_sha256: None,
    };
    mgr.state
        .plugins
        .insert(id_a.clone(), vec![mk(PluginScope::User, "/a")]);
    mgr.state
        .plugins
        .insert(id_b.clone(), vec![mk(PluginScope::User, "/b")]);
    mgr.state
        .plugins
        .insert(id_c.clone(), vec![mk(PluginScope::User, "/c")]);

    // aaa + ccc 在 enabled 集合。
    let enabled: HashSet<String> = ["aaa@inline", "ccc@inline"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let out = mgr.list_with_enabled(&enabled);
    // 排序后应见 (aaa, true), (bbb, false), (ccc, true)。
    let flags: Vec<(&str, bool)> = out.iter().map(|(id, _, on)| (id.as_str(), *on)).collect();
    assert_eq!(
        flags,
        vec![
            ("aaa@inline", true),
            ("bbb@inline", false),
            ("ccc@inline", true),
        ]
    );
}

#[test]
fn lookup_entry_returns_highest_precedence_scope() {
    // entries_for 已按 scope precedence 降序排,lookup_entry 取首项。
    let tmp = TempDir::new().unwrap();
    let mut mgr = PluginManager::new(tmp.path());
    let id = PluginId::inline("demo").unwrap();
    mgr.state.plugins.insert(
        id.clone(),
        vec![
            InstallationEntry {
                scope: PluginScope::User,
                install_path: PathBuf::from("/u"),
                version: "1.0.0".into(),
                installed_at: Utc::now(),
                last_updated: Utc::now(),
                git_commit_sha: None,
                checksum_sha256: None,
            },
            InstallationEntry {
                scope: PluginScope::Local,
                install_path: PathBuf::from("/l"),
                version: "1.1.0".into(),
                installed_at: Utc::now(),
                last_updated: Utc::now(),
                git_commit_sha: None,
                checksum_sha256: None,
            },
        ],
    );
    let entry = mgr.lookup_entry(&id).unwrap();
    assert_eq!(entry.scope, PluginScope::Local);
    assert_eq!(entry.version, "1.1.0");
}

#[test]
fn lookup_entry_returns_none_for_unknown() {
    let tmp = TempDir::new().unwrap();
    let mgr = PluginManager::new(tmp.path());
    let id = PluginId::inline("ghost").unwrap();
    assert!(mgr.lookup_entry(&id).is_none());
}

// ── v1.0.0-rc3:install_from_marketplace ──────────────────────────

/// 构造一个只含单个 marketplace 入口的 `KnownMarketplacesFile` —— 供
/// `install_from_marketplace` 的 Phase F 签名使用。
fn make_known_with_one(
    mkt_name: &MarketplaceName,
    install_location: PathBuf,
) -> KnownMarketplacesFile {
    let mut f = KnownMarketplacesFile::new();
    f.upsert(
        mkt_name.clone(),
        crate::state::KnownMarketplace {
            source: crate::manifest::MarketplaceSource::Directory {
                path: install_location.clone(),
            },
            install_location,
            last_updated: Utc::now(),
            auto_update: false,
        },
    );
    f
}

/// 构造一个本地 marketplace 根目录,带 `.claude-plugin/marketplace.json` 和一个
/// `Directory` 类型的 plugin entry。返回 `(marketplace_root, marketplace_name)`。
fn make_marketplace_fixture(
    parent: &Path,
    mkt_name: &str,
    plugin_name: &str,
) -> (PathBuf, MarketplaceName) {
    let mkt_root = parent.join(mkt_name);
    fs::create_dir_all(mkt_root.join(".claude-plugin")).unwrap();
    let plugin_dir = mkt_root.join("plugins").join(plugin_name);
    fs::create_dir_all(&plugin_dir).unwrap();
    fs::write(
        plugin_dir.join("plugin.toml"),
        format!(
            "name = \"{plugin_name}\"\nversion = \"2.0.0\"\ndescription = \"from marketplace\"\n"
        ),
    )
    .unwrap();

    let manifest_json = format!(
        r#"{{
            "name": "{mkt_name}",
            "owner": {{ "name": "Test" }},
            "plugins": [
                {{
                    "name": "{plugin_name}",
                    "version": "2.0.0",
                    "source": {{ "source": "directory", "path": "./plugins/{plugin_name}" }}
                }}
            ]
        }}"#
    );
    fs::write(
        mkt_root.join(".claude-plugin").join("marketplace.json"),
        manifest_json,
    )
    .unwrap();
    let name = MarketplaceName::parse(mkt_name).unwrap();
    (mkt_root, name)
}

#[test]
fn install_from_marketplace_writes_to_cache_under_named_marketplace() {
    let tmp = TempDir::new().unwrap();
    let (mkt_root, mkt_name) = make_marketplace_fixture(tmp.path(), "official", "demo");

    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    let known = make_known_with_one(&mkt_name, mkt_root.clone());
    let report = mgr
        .install_from_marketplace(&known, &mkt_root, &mkt_name, "demo", PluginScope::User)
        .unwrap();

    // 成功聚合:1 个 installed,0 个 failed。
    assert!(report.is_success(), "report: {report:?}");
    assert_eq!(report.installed.len(), 1);
    let (id, entry) = &report.installed[0];
    // Plugin id 应是 demo@official(marketplace 段来自 MarketplaceName 参数)。
    assert_eq!(id.as_str(), "demo@official");
    // cache layout 应落在 cache/official/demo/<version>/
    assert!(entry.install_path.ends_with("cache/official/demo/2.0.0"));
    assert!(entry.install_path.exists());
    // plugin.toml 被复制。
    assert!(entry.install_path.join("plugin.toml").exists());
}

#[test]
fn install_from_marketplace_aggregates_unknown_plugin_failure() {
    // Phase F:找不到的 plugin 走 `report.failed` 而非 `Err` —— 让 dep closure
    // 中其他能装上的也保留。fail-fast 留给 manifest 完全缺失场景。
    let tmp = TempDir::new().unwrap();
    let (mkt_root, mkt_name) = make_marketplace_fixture(tmp.path(), "official", "demo");

    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    let known = make_known_with_one(&mkt_name, mkt_root.clone());
    let report = mgr
        .install_from_marketplace(&known, &mkt_root, &mkt_name, "ghost", PluginScope::User)
        .unwrap();
    assert!(!report.is_success(), "expected failure, got {report:?}");
    assert_eq!(report.installed.len(), 0);
    assert_eq!(report.failed.len(), 1);
    assert!(report.failed[0].target.contains("ghost"));
    match &report.failed[0].error {
        PluginError::NotInstalled(msg) => assert!(msg.contains("ghost")),
        other => panic!("expected NotInstalled, got {other:?}"),
    }
}

#[test]
fn install_from_marketplace_errors_on_missing_manifest() {
    // marketplace 根目录存在但缺 `.claude-plugin/marketplace.json`。
    // Phase F 仍 fail-fast 返回 `Err`(没法走 dep resolver)。
    let tmp = TempDir::new().unwrap();
    let empty_root = tmp.path().join("empty-mkt");
    fs::create_dir_all(&empty_root).unwrap();
    let mkt_name = MarketplaceName::parse("empty").unwrap();

    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    let known = make_known_with_one(&mkt_name, empty_root.clone());
    let err = mgr
        .install_from_marketplace(&known, &empty_root, &mkt_name, "demo", PluginScope::User)
        .unwrap_err();
    assert!(matches!(err, PluginError::MarketplaceManifestNotFound(_)));
}

#[test]
fn install_from_marketplace_aggregates_non_local_source_failure() {
    // marketplace entry 的 source 是 Git(非 File/Directory) → Phase D 仍拒绝,
    // 失败进 `report.failed`(root 自身),不再 throw。
    let tmp = TempDir::new().unwrap();
    let mkt_root = tmp.path().join("git-mkt");
    fs::create_dir_all(mkt_root.join(".claude-plugin")).unwrap();
    let manifest_json = r#"{
        "name": "git-mkt",
        "owner": { "name": "X" },
        "plugins": [
            {
                "name": "remote",
                "version": "1.0.0",
                "source": {
                    "source": "git",
                    "url": "https://example.com/repo.git"
                }
            }
        ]
    }"#;
    fs::write(
        mkt_root.join(".claude-plugin").join("marketplace.json"),
        manifest_json,
    )
    .unwrap();
    let mkt_name = MarketplaceName::parse("git-mkt").unwrap();

    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    let known = make_known_with_one(&mkt_name, mkt_root.clone());
    let report = mgr
        .install_from_marketplace(&known, &mkt_root, &mkt_name, "remote", PluginScope::User)
        .unwrap();
    assert!(!report.is_success(), "expected failure, got {report:?}");
    assert_eq!(report.installed.len(), 0);
    assert_eq!(report.failed.len(), 1);
    assert!(matches!(
        report.failed[0].error,
        PluginError::MarketplaceFetch { .. }
    ));
}

#[test]
fn install_from_marketplace_resolves_relative_paths() {
    // entry source 是 `./plugins/demo`(相对路径) → resolve 后变成绝对。
    let tmp = TempDir::new().unwrap();
    let (mkt_root, mkt_name) = make_marketplace_fixture(tmp.path(), "rel-mkt", "demo");

    let plugins_root = tmp.path().join("plugins");
    let mut mgr = PluginManager::new(&plugins_root);
    let known = make_known_with_one(&mkt_name, mkt_root.clone());
    let report = mgr
        .install_from_marketplace(&known, &mkt_root, &mkt_name, "demo", PluginScope::User)
        .unwrap();
    assert!(report.is_success(), "report: {report:?}");
    let (_, entry) = &report.installed[0];
    // install_path 是 cache/rel-mkt/demo/<version>/,且真实存在。
    assert!(entry.install_path.exists());
    assert!(entry.install_path.ends_with("cache/rel-mkt/demo/2.0.0"));
}
