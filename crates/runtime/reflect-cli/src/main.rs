//! reflect —— 顶层 CLI 路由器。分发 `exec` / `discussion` /
//! `login` / `version` 等子命令。
//!
//! 注:交互式 TUI 已拆分至独立仓库 Reflect-TUI
//! (https://cnb.cool/Demon1019/Reflect-CLI),核心仓库的 `reflect` 二进制
//! 不再含 `tui` 子命令。headless / CLI 工具类子命令仍在此提供。

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use reflect_exec::ExecArgs;
use reflect_exec::serve::ServeArgs;
use reflect_plugin::state::PluginScope;

mod config;
mod doctor;
mod login;
mod lsp;
mod mcp;
mod pipeline;
mod plugin;
mod security;
mod session;
mod task;
mod traces;
mod update;
mod workspace;

#[cfg(test)]
mod test_home;

#[derive(Debug, Parser)]
#[command(
    name = "reflect",
    version,
    about = "Reflect — Rust agent runtime CLI (subcommands: exec, serve, discussion, login, mcp, config, session, traces, doctor, plugin, lsp, task, pipeline, security, workspace, update, version). 交互式 TUI 见独立仓库 Reflect-TUI.",
    propagate_version = true
)]
struct Cli {
    /// 工作目录(默认:当前目录)。`global = true` 使其对所有子命令生效
    /// (`reflect --cwd X exec`)。在 dispatch 前通过 `std::env::set_current_dir`
    /// 应用,使子命令内部的 `current_dir()` 调用一致。
    #[arg(long, short = 'C', value_name = "PATH", global = true)]
    cwd: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 执行单轮 headless 对话,并以 JSONL Event 流输出。
    Exec {
        #[command(flatten)]
        args: ExecArgs,
    },
    /// v1.3 SDK:常驻 stdio JSONL 会话服务(Python / TS SDK 的协议入口)。
    /// stdin 逐行读 Submission,stdout 逐行写 Event;多轮共享同一 session。
    Serve {
        #[command(flatten)]
        args: ServeArgs,
    },
    /// 按 TOML 配置文件运行多 agent 讨论。
    Discussion {
        #[command(subcommand)]
        action: DiscussionAction,
    },
    /// 配置 provider 凭据(OpenAI / Anthropic / Ollama):将 API key 写入
    /// `~/.reflect/config.toml`。v0.4 替换 v0.3 的 stub。
    Login {
        /// Provider 名,默认 `anthropic`。可取值:anthropic | openai
        /// | ollama(`local` 为 `ollama` 的别名)。
        #[arg(long, default_value = "anthropic")]
        provider: String,
        /// 通过 flag 而非 stdin 传入 API key(便于脚本调用)。
        /// 若 flag 与 stdin 都不可用,调用报错。
        #[arg(long)]
        api_key: Option<String>,
        /// 跳过「key 过短」的告警(CI / 脚本化登录场景)。
        #[arg(long)]
        force: bool,
    },
    /// 列出 / 新增 / 删除 / 测试 / 显示 MCP server 配置。
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// 查看或编辑 `~/.reflect/config.toml`(show / set / unset / edit / ls)。
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// 列出 / 查看 / 删除持久化 session。
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// v1.2 P1:列出 / 查看本地 LLM 调用记录(`~/.reflect/traces/model-io/`)。
    Traces {
        #[command(subcommand)]
        action: TracesAction,
    },
    /// 尽力而为的环境自检(config / rollout / mcp / rustc)。
    Doctor {
        /// 同时对当前 provider 发一次 1 token 请求探测(会消耗 token)。
        #[arg(long)]
        check_network: bool,
    },
    /// 安装 / 列出 / 启用 / 禁用 / 卸载插件。
    /// v1.0.0-rc2:plugin 主体 7 条 + marketplace stub 3 条。
    Plugin {
        #[command(subcommand)]
        action: PluginAction,
    },
    /// v0.5:列出 / 查看 LSP server 配置。
    Lsp {
        #[command(subcommand)]
        action: LspAction,
    },
    /// v1.1.0:结构化任务 + 团队管理(TaskCreate / TaskList / TeamCreate 等)。
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// v1.1.0:运行 DAG 流水线(team-plan → team-prd → team-exec → team-verify)。
    Pipeline {
        #[command(subcommand)]
        action: PipelineAction,
    },
    /// P2:安全扫描(cargo audit)。
    Security {
        #[command(subcommand)]
        action: SecurityAction,
    },
    /// P3:workspace 的 git clone / sync。
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// 打印升级说明(v0.4:不联网;v0.5 会查 GitHub)。
    Update {
        /// 预留给 v0.5 的 GitHub release API 检查。
        #[arg(long)]
        check_only: bool,
    },
    /// 打印版本号并退出。
    Version,
}

/// `reflect mcp` 子命令。
#[derive(Debug, Subcommand)]
enum McpAction {
    /// 列出已配置的 MCP server。
    Ls,
    /// 新增 MCP server。`--command`(stdio)与 `--url`(http)互斥。
    Add {
        /// Server 名(`[mcp_servers]` 下的 key)。
        name: String,
        /// stdio 模式:要派生的命令。
        #[arg(long, conflicts_with = "url")]
        command: Option<String>,
        /// stdio 模式:命令参数。
        #[arg(long, num_args = 0..)]
        args: Vec<String>,
        /// http 模式:server URL。
        #[arg(long, conflicts_with = "command")]
        url: Option<String>,
        /// http 模式:`Key: Value` 形式的额外 header(可重复)。
        #[arg(long, num_args = 0..)]
        header: Vec<String>,
        /// 单次工具调用超时(毫秒)。
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
    },
    /// 删除已配置的 MCP server。
    Remove { name: String },
    /// P3:浏览精选 MCP registry catalog。
    Registry {
        #[command(subcommand)]
        action: McpRegistryAction,
    },
    /// 启动 server 并列出它的工具(超时 3 秒;用于验证配置可用)。
    Test { name: String },
    /// 美化输出某个 server 的完整配置。
    Show { name: String },
}

/// `reflect mcp registry` 子命令 —— 精选 catalog stub。
#[derive(Debug, Subcommand)]
enum McpRegistryAction {
    /// 列出精选 MCP server 模板。
    Ls,
    /// 显示某个 catalog id 的安装提示。
    Show { id: String },
}

/// `reflect security` 子命令。
#[derive(Debug, Subcommand)]
enum SecurityAction {
    /// 在 workspace 中运行 `cargo audit`。
    Audit {
        /// workspace 根目录(默认:当前目录)。
        #[arg(long)]
        path: Option<PathBuf>,
    },
}

/// `reflect workspace` 子命令。
#[derive(Debug, Subcommand)]
enum WorkspaceAction {
    /// 显示 workspace sync 状态提示。
    Ls,
    /// 将 git 仓库克隆到本地目录。
    Clone {
        url: String,
        /// 目标目录。
        #[arg(long, short = 'd')]
        dest: PathBuf,
        /// 分支或 tag。
        #[arg(long, short = 'b')]
        branch: Option<String>,
        /// 浅克隆深度(0 = 全量克隆)。
        #[arg(long, default_value_t = 0)]
        depth: u32,
    },
}

/// `reflect config` 子命令。
#[derive(Debug, Subcommand)]
enum ConfigAction {
    /// 打印生效配置(除非 `--reveal`,否则 api_key 会脱敏)。
    Show {
        /// 以明文打印 api_key。
        #[arg(long)]
        reveal: bool,
    },
    /// 设置单个配置项,例如 `anthropic.model claude-3-haiku-20240307`。
    Set { key: String, value: String },
    /// 将配置项重置为默认值(None / 空)。
    Unset { key: String },
    /// 用 `$EDITOR` 打开 `~/.reflect/config.toml`(回退到 `vi`)。
    Edit,
    /// 列出已知配置项及其当前值(api_key 脱敏)。
    Ls,
}

/// `reflect session` 子命令。
#[derive(Debug, Subcommand)]
enum SessionAction {
    /// 列出 `~/.reflect/sessions/` 下的近期 session。
    Ls {
        /// 最多显示行数。
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
        /// 仅显示 `model` 字段包含此子串的行。
        #[arg(long)]
        model: Option<String>,
    },
    /// 显示单个 session 的元数据 + 前几条消息(支持完整 UUID 或前 N 字符)。
    Show { id: String },
    /// 删除一个 session 文件(除非 `--yes`,否则先确认)。
    Rm {
        id: String,
        /// 跳过 y/N 确认提示。
        #[arg(long)]
        yes: bool,
    },
    /// fork 一个 session:把完整历史复制到一个新子 session(v1.2 P2)。
    /// 用 `reflect exec --resume <child_id>` 继续子 session。
    Fork {
        /// 父 session id(完整 UUID 或唯一前缀)。
        id: String,
        /// 写入 Fork 标记中的可选分支名(默认 "manual")。
        #[arg(long)]
        branch: Option<String>,
    },
    /// 重命名 session:在 `ls`/`show` 中显示一个可读名称
    ///(覆盖自动派生的标题)。v1.x S5b。
    Rename {
        /// Session id(完整 UUID 或唯一前缀)。
        id: String,
        /// session 的新名称。
        name: String,
    },
    /// 将 session 导出为可读 markdown(输出到 stdout 或 `--out <file>`)。
    /// v1.x S3:渲染完整对话(用户 + 助手 + 工具调用)。
    Export {
        /// Session id(完整 UUID 或唯一前缀)。
        id: String,
        /// 写入该文件而非 stdout。
        #[arg(long, short = 'o')]
        out: Option<std::path::PathBuf>,
    },
}

/// `reflect traces ...` —— 列 / 看本地 LLM 调用记录(v1.2 P1)。
///
/// 数据来自 `reflect-telemetry` 的 model-io JSONL(每次 LLM 调用一条记录)。
/// 与 `reflect session`(rollout 会话记录)正交 —— 一个会话可能产生多次
/// LLM 调用,traces 关注的是「模型 I/O 明细」。
#[derive(Debug, Subcommand)]
enum TracesAction {
    /// 列出 `~/.reflect/traces/model-io/` 下的近期 LLM trace session。
    Ls {
        /// 最多显示行数。
        #[arg(long, short = 'n', default_value_t = 20)]
        limit: usize,
    },
    /// 显示一个 session 内的全部 LLM 调用(支持完整 UUID 或前 N 字符)。
    Show { id: String },
}

/// `reflect task ...` —— 结构化任务 + 团队管理 CLI(v1.1.0 落地)。
///
/// 直接连 `TaskManager`(FileTaskStore + FileTeamStore),不经过 Tool execute
/// 路径,与 `reflect session` / `reflect mcp` 等设计对称。
#[derive(Debug, Subcommand)]
enum TaskAction {
    /// 列出 list 下的任务(默认扫所有 list)。
    Ls {
        /// 限定到单一 list(worker 视角 = session id)。
        #[arg(long)]
        list: Option<String>,
        /// 包含软删除的任务。
        #[arg(long)]
        include_deleted: bool,
    },
    /// 显示单条任务详情。
    Show {
        /// 列表 id。
        #[arg(long)]
        list: String,
        /// 任务 id。
        task_id: u32,
    },
    /// 创建任务。
    Create {
        /// 简短标题(必填)。
        #[arg(long)]
        subject: String,
        /// 详细描述。
        #[arg(long, default_value = "")]
        description: String,
        /// List id;默认 = 当前 session id(由环境推断)。
        #[arg(long, default_value = "")]
        list: String,
        /// 进行时形态(如 "Implementing")。
        #[arg(long)]
        active_form: Option<String>,
    },
    /// 更新任务字段(目前支持 status / subject)。
    Update {
        /// 任务 id。
        task_id: u32,
        /// 列表 id。
        #[arg(long)]
        list: String,
        /// 新状态(pending / in_progress / completed / deleted)。
        #[arg(long)]
        status: Option<String>,
        /// 新标题(subject 字段)。
        #[arg(long)]
        subject: Option<String>,
    },
    /// 物理删除 task 文件(不可恢复;软删除请用 `update --status deleted`)。
    Stop {
        /// 任务 id。
        task_id: u32,
        /// 列表 id。
        #[arg(long)]
        list: String,
    },
    /// 级联清理 task 文件。`TeamDelete` 不删 tasks,留待本命令显式清理。
    Purge {
        /// 删除 `<home>/tasks/<team>/` 整个目录。
        #[arg(long)]
        team: Option<String>,
        /// `--team` 别名(同语义,优先 team)。
        #[arg(long)]
        list: Option<String>,
        /// 跳过 y/N 确认。
        #[arg(long)]
        yes: bool,
    },
    /// 团队子命令。
    Team {
        #[command(subcommand)]
        action: TeamAction,
    },
}

/// `reflect task team ...` 子命令。
#[derive(Debug, Subcommand)]
enum TeamAction {
    /// 列出所有 team(按 name 字典序)。
    Ls,
    /// 显示单个 team 详情。
    Show { name: String },
    /// 创建一个 team(仅 lead,默认)。
    Create {
        /// Team 名(字符集 `[a-z0-9_-]+`)。
        name: String,
        /// 描述。
        #[arg(long)]
        description: Option<String>,
    },
    /// 物理删除 team 文件(不级联删 tasks —— 用 `reflect task purge --team <name>`)。
    Delete {
        /// Team 名。
        name: String,
        /// 跳过 y/N 确认。
        #[arg(long)]
        yes: bool,
    },
    /// 把所有 team 成员 spec 同步到 SubAgentFactory(`--list-specs` 仅打印 list_specs)。
    Sync {
        /// 同步后只列出 factory.list_specs(),不打印 sync 摘要。
        #[arg(long)]
        list_specs: bool,
    },
}

/// `reflect discussion` 子命令:`run` 主跑流程,`ls` 列已完成的讨论。
#[derive(Debug, Subcommand)]
enum DiscussionAction {
    /// 端到端运行一次讨论,并打印 transcript 与结果。
    Run {
        /// discussion.toml 的路径。
        #[arg(long, short = 'c')]
        config: PathBuf,
        /// 可选输出文件(默认:stdout)。
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },
    /// v0.2.4:从 JSONL rollout 列出历史讨论。带 `--discussion-id` 时
    /// 仅列包含该 discussion id 的 session;不带时列出至少含一条
    /// `DiscussionTranscript` 记录的所有 session。
    Ls {
        /// 可选:仅列出包含该 discussion id(UUID)的 session。
        #[arg(long)]
        discussion_id: Option<String>,
    },
}

/// `reflect pipeline` 子命令(v1.1.0 Phase 5):运行 DAG 流水线。
#[derive(Debug, Subcommand)]
enum PipelineAction {
    /// 端到端运行 DAG 流水线,并打印报告(可选写入文件)。
    Run {
        /// pipeline.toml 的路径。
        #[arg(long, short = 'c')]
        config: PathBuf,
        /// 流水线主题;模板 `{{topic}}` 可引用之。
        #[arg(long)]
        topic: String,
        /// 覆盖失败策略(`abort` / `continue_collect`)。
        /// 缺省 = config 中指定,默认 `abort`。
        #[arg(long)]
        failure_policy: Option<String>,
        /// 可选 JSON 输出路径(默认:仅人可读 stdout)。
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },
    /// 为指定 team 运行预设流水线(plan → prd → exec → verify)。
    /// v1.x:接入此前孤儿的 `run_team_pipeline` / `TEAM_PIPELINE_TOML`。
    Team {
        /// Team 名(必须存在于 ~/.reflect/teams/)。
        name: String,
        /// 流水线主题;模板 `{{topic}}` 可引用之。
        #[arg(long)]
        topic: String,
    },
}

/// `reflect plugin` 主体的 7 条子命令 + marketplace 3 条 stub。
#[derive(Debug, Subcommand)]
enum PluginAction {
    /// 从本地目录安装插件(manifest + payload)。
    Install {
        /// 插件源码目录路径。
        path: PathBuf,
        /// 安装作用域(默认:user)。
        #[arg(long, value_enum, default_value_t = CliPluginScope::User)]
        scope: CliPluginScope,
    },
    /// 列出已安装插件(与 config 中的 enabled_plugins 合并)。
    List,
    /// 显示某个插件的元数据(scope / version / install_path / status)。
    Info {
        /// `<name>@<marketplace>` 形式的 plugin id。
        id: String,
    },
    /// 将 plugin id 加入 `config.toml#plugins.enabled_plugins`。
    Enable {
        /// `<name>@<marketplace>` 形式的 plugin id。
        id: String,
    },
    /// 从 `config.toml#plugins.enabled_plugins` 移除 plugin id。
    Disable {
        /// `<name>@<marketplace>` 形式的 plugin id。
        id: String,
    },
    /// 从 `installed_plugins.json` 与启用列表移除条目。
    Uninstall {
        /// `<name>@<marketplace>` 形式的 plugin id。
        id: String,
        /// 要移除的作用域(必须与最初安装作用域一致)。
        #[arg(long, value_enum, default_value_t = CliPluginScope::User)]
        scope: CliPluginScope,
        /// 跳过 y/N 确认提示。
        #[arg(long)]
        yes: bool,
    },
    /// 美化输出某个插件的 `plugin.toml`(manifest 内容)。
    Show {
        /// `<name>@<marketplace>` 形式的 plugin id。
        id: String,
    },
    /// 管理已知 marketplace(Phase D stub:暂不执行 fetch)。
    Marketplace {
        #[command(subcommand)]
        action: MarketplaceAction,
    },
}

/// `reflect lsp` 子命令(v0.5 Phase A:list + status)。
#[derive(Debug, Subcommand)]
enum LspAction {
    /// 列出已配置的 LSP server。
    List,
    /// 显示某个 LSP server 的完整配置。
    Status { name: String },
}

/// `reflect plugin marketplace` 子命令(Phase D:Git/File/Directory 真接通,Phase E:Url/Github/refresh)。
#[derive(Debug, Subcommand)]
enum MarketplaceAction {
    /// 列出已知 marketplace。
    Ls,
    /// 新增已知 marketplace。Phase D 真 fetch(Git/File/Directory),
    /// Phase E:Url/Github 也真接通。
    Add {
        /// marketplace 名(小写 kebab-case)。
        name: String,
        /// source 类型。
        #[arg(long, value_enum)]
        from: MarketplaceSourceKind,
        /// `--from github`:owner/name(例 `anthropics/claude-plugins-official`)。
        #[arg(long)]
        repo: Option<String>,
        /// `--from git` / `--from url`:远程 URL。
        #[arg(long)]
        url: Option<String>,
        /// `--from git`:branch / tag 名(可选,默认走 HEAD)。
        #[arg(long)]
        r#ref: Option<String>,
        /// `--from git`:精确 commit sha(可选,与 --ref 互斥语义)。
        #[arg(long)]
        sha: Option<String>,
        /// `--from file` / `--from directory`:本地路径。
        #[arg(long)]
        path: Option<String>,
    },
    /// 删除已知 marketplace(Phase D:真删 cache)。
    Remove {
        /// marketplace 名。
        name: String,
    },
    /// Phase E:触发一个或全部 marketplace 的增量刷新(覆盖已知 cache)。
    /// 不传 name = 刷全部 `auto_update = true` 的 marketplace。
    Refresh {
        /// 不传 = 刷全部 `auto_update = true` 的 marketplace。
        #[arg(long)]
        name: Option<String>,
    },
}

/// CLI 镜像 `reflect_plugin::state::PluginScope`,给 clap derive。
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CliPluginScope {
    Managed,
    User,
    Project,
    Local,
}

impl From<CliPluginScope> for PluginScope {
    fn from(s: CliPluginScope) -> Self {
        match s {
            CliPluginScope::Managed => PluginScope::Managed,
            CliPluginScope::User => PluginScope::User,
            CliPluginScope::Project => PluginScope::Project,
            CliPluginScope::Local => PluginScope::Local,
        }
    }
}

/// `--from` 的镜像 enum。Phase D:Git/File/Directory 真接通,Github/Url 报 Phase E stub。
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum MarketplaceSourceKind {
    Github,
    Git,
    Url,
    File,
    Directory,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // 全局 `--cwd` / `-C`:最早应用,后续 `current_dir()` / 配置路径都基于此。
    // 失败给出可读错误而非裸 IO 错误。
    if let Some(p) = &cli.cwd {
        std::env::set_current_dir(p).map_err(|e| anyhow::anyhow!("--cwd {}: {e}", p.display()))?;
    }
    // 无子命令时提示用法 —— 交互式 TUI 已拆分至独立仓库 Reflect-TUI。
    let command = match cli.command {
        Some(c) => c,
        None => {
            println!("Reflect agent runtime CLI。可用子命令见 `reflect --help`。");
            println!();
            println!("交互式 TUI 已独立:见 https://cnb.cool/Demon1019/Reflect-CLI");
            println!("headless 单轮对话:`reflect exec \"你的问题\"`");
            return Ok(());
        }
    };
    let rt = tokio::runtime::Runtime::new()?;
    match command {
        Command::Exec { args } => reflect_exec::run(args),
        Command::Serve { args } => reflect_exec::serve::run_serve(args),
        Command::Discussion { action } => match action {
            DiscussionAction::Run { config, output } => rt
                .block_on(reflect_discussion::cli::run(&config, output.as_deref()))
                .map_err(anyhow::Error::from),
            DiscussionAction::Ls { discussion_id } => {
                let base = reflect_rollout::path::default_base();
                let sessions = if let Some(id) = discussion_id.as_deref() {
                    reflect_rollout::index::list_sessions_with_discussion(&base, id)?
                } else {
                    // 未指定时 fallback 到普通 list_sessions(v0.2.4 简化为"列所有
                    // session"语义,因为 list_sessions_with_discussion 至少需要
                    // 一个 DiscussionTranscript 才匹配)。
                    reflect_rollout::index::list_sessions(&base)?
                };
                if sessions.is_empty() {
                    println!("(no sessions found under {})", base.display());
                    return Ok(());
                }
                println!(
                    "{:<36}  {:<32}  {:<12}  {:>6}",
                    "session_id", "model", "started", "msgs"
                );
                for s in &sessions {
                    let model = truncate(&s.model, 32);
                    println!(
                        "{:<36}  {:<32}  {:<12}  {:>6}",
                        s.session_id.to_string(),
                        model,
                        s.started_at.format("%Y-%m-%d"),
                        s.message_count
                    );
                }
                Ok(())
            }
        },
        Command::Login {
            provider,
            api_key,
            force,
        } => login::run(&provider, api_key, force),
        Command::Mcp { action } => match action {
            McpAction::Ls => mcp::ls(),
            McpAction::Add {
                name,
                command,
                args,
                url,
                header,
                timeout_ms,
            } => mcp::add(&name, command, args, url, header, timeout_ms),
            McpAction::Remove { name } => mcp::remove(&name),
            McpAction::Registry { action } => match action {
                McpRegistryAction::Ls => mcp::registry_ls(),
                McpRegistryAction::Show { id } => mcp::registry_show(&id),
            },
            McpAction::Show { name } => mcp::show(&name),
            McpAction::Test { name } => {
                let rt = tokio::runtime::Runtime::new()?;
                rt.block_on(mcp::test(&name))
            }
        },
        Command::Config { action } => match action {
            ConfigAction::Show { reveal } => config::show(reveal),
            ConfigAction::Set { key, value } => config::set(&key, &value),
            ConfigAction::Unset { key } => config::unset(&key),
            ConfigAction::Edit => config::edit(),
            ConfigAction::Ls => config::ls(),
        },
        Command::Session { action } => match action {
            SessionAction::Ls { limit, model } => session::ls(limit, model.as_deref()),
            SessionAction::Show { id } => session::show(&id),
            SessionAction::Rm { id, yes } => session::rm(&id, yes),
            SessionAction::Fork { id, branch } => session::fork(&id, branch.as_deref()),
            SessionAction::Rename { id, name } => session::rename(&id, &name),
            SessionAction::Export { id, out } => session::export(&id, out.as_deref()),
        },
        Command::Traces { action } => match action {
            TracesAction::Ls { limit } => traces::ls(limit),
            TracesAction::Show { id } => traces::show(&id),
        },
        Command::Doctor { check_network } => doctor::run(check_network),
        Command::Update { check_only } => update::run(check_only),
        Command::Plugin { action } => match action {
            PluginAction::Install { path, scope } => {
                rt.block_on(plugin::install(&path, scope.into()))
            }
            PluginAction::List => plugin::list(),
            PluginAction::Info { id } => plugin::info(&id),
            PluginAction::Enable { id } => plugin::enable(&id),
            PluginAction::Disable { id } => plugin::disable(&id),
            PluginAction::Uninstall { id, scope, yes } => plugin::uninstall(&id, scope.into(), yes),
            PluginAction::Show { id } => plugin::show_manifest(&id),
            PluginAction::Marketplace { action: ma } => match ma {
                MarketplaceAction::Ls => plugin::marketplace_ls(),
                MarketplaceAction::Add {
                    name,
                    from,
                    repo,
                    url,
                    r#ref,
                    sha,
                    path,
                } => rt.block_on(plugin::marketplace_add(
                    &name,
                    from,
                    repo.as_deref(),
                    url.as_deref(),
                    r#ref.as_deref(),
                    sha.as_deref(),
                    path.as_deref(),
                )),
                MarketplaceAction::Remove { name } => plugin::marketplace_remove(&name),
                MarketplaceAction::Refresh { name } => {
                    rt.block_on(plugin::marketplace_refresh(name.as_deref()))
                }
            },
        },
        Command::Lsp { action } => match action {
            LspAction::List => lsp::list(),
            LspAction::Status { name } => lsp::status(&name),
        },
        Command::Task { action } => match action {
            TaskAction::Ls {
                list,
                include_deleted,
            } => rt.block_on(task::ls(list.as_deref(), include_deleted)),
            TaskAction::Show { list, task_id } => rt.block_on(task::show(&list, task_id)),
            TaskAction::Create {
                subject,
                description,
                list,
                active_form,
            } => {
                // 空 list → fallback 到 "$USER" 或 "default"(CLI 无 session 上下文)。
                let list_id = if list.is_empty() {
                    std::env::var("USER")
                        .or_else(|_| std::env::var("USERNAME"))
                        .unwrap_or_else(|_| "default".to_string())
                } else {
                    list
                };
                rt.block_on(task::create(
                    &subject,
                    &description,
                    &list_id,
                    active_form.as_deref(),
                ))
            }
            TaskAction::Update {
                task_id,
                list,
                status,
                subject,
            } => rt.block_on(task::update(
                task_id,
                &list,
                status.as_deref(),
                subject.as_deref(),
            )),
            TaskAction::Stop { task_id, list } => rt.block_on(task::stop(&list, task_id)),
            TaskAction::Purge { team, list, yes } => {
                task::purge(team.as_deref(), list.as_deref(), yes)
            }
            TaskAction::Team { action: ta } => match ta {
                TeamAction::Ls => rt.block_on(task::team_ls()),
                TeamAction::Show { name } => rt.block_on(task::team_show(&name)),
                TeamAction::Create { name, description } => {
                    rt.block_on(task::team_create(&name, description.as_deref()))
                }
                TeamAction::Delete { name, yes } => rt.block_on(task::team_delete(&name, yes)),
                TeamAction::Sync { list_specs } => rt.block_on(task::team_sync(list_specs)),
            },
        },
        Command::Pipeline { action } => match action {
            PipelineAction::Run {
                config,
                topic,
                failure_policy,
                output,
            } => rt.block_on(pipeline::run(
                &config,
                &topic,
                failure_policy.as_deref(),
                output.as_deref(),
            )),
            PipelineAction::Team { name, topic } => rt.block_on(pipeline::run_team(&name, &topic)),
        },
        Command::Security { action } => match action {
            SecurityAction::Audit { path } => security::run_audit(path),
        },
        Command::Workspace { action } => match action {
            WorkspaceAction::Ls => workspace::run_ls_hint(),
            WorkspaceAction::Clone {
                url,
                dest,
                branch,
                depth,
            } => workspace::run_clone(url, dest, branch, depth),
        },
        Command::Version => {
            println!("reflect {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
    }
}

/// 字符串按 UTF-8 字符边界截断到 `max_chars`,超出追加 `…`。
fn truncate(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, c) in s.chars().enumerate() {
        if count >= max_chars.saturating_sub(1) {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}
