//! Marketplace fetch / refresh 抽象 —— Phase D 范围(底层) + Phase E(上层)。
//!
//! 把不同来源(Git / File / Directory / Url / Github)的 marketplace 物化到
//! 本地 cache,供后续 `MarketplaceManifest::from_json_path` 与
//! `install_from_marketplace` 复用。
//!
//! 设计要点:
//! - **trait + router** —— `MarketplaceFetcher` 是双方法 trait(fetch + update),
//!   `MarketplaceFetchRouter` 按 `MarketplaceSource` variant 分派;
//!   `MarketplaceUpdater` 持有 router + 提供高层 `refresh_one` / `refresh_all`。
//! - **零 IO 接口 / 真 IO 实现分离** —— `DirectoryFetcher::fetch` 是 0 IO
//!   (只验证 manifest 路径存在);`GitFetcher` / `FileFetcher` / `UrlFetcher` 才
//!   触发真 fetch。
//! - **Git 走 shell `git` CLI** —— 不引入 libgit2 原生依赖,
//!   `--depth 1 --filter=blob:none` 单 commit 浅克隆;`update` 走
//!   `git pull --ff-only`。
//! - **Github = Git 转委托** —— `GithubFetcher::to_git` 把
//!   `owner/repo` 拼成 `https://github.com/<repo>.git`,完全复用 `GitFetcher`。
//! - **TUI 不阻断启动** —— `MarketplaceUpdater::refresh_all` 配合 `tokio::spawn`,
//!   单个 marketplace 失败只 `tracing::warn!`,不影响 TUI 主循环。

pub mod fetcher;
pub mod update;

pub use fetcher::{
    DirectoryFetcher, FileFetcher, GitFetcher, GithubFetcher, MarketplaceFetchRouter,
    MarketplaceFetcher, UrlFetcher,
};
pub use update::MarketplaceUpdater;
