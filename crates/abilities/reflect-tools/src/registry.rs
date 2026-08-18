//! `ToolRegistry` —— name → `Arc<dyn Tool>` 查询,带 source 标记。
//!
//! v1.3 起支持 `Builtin | Runtime | Plugin | Mcp` 四种 source tag。
//! Plugin 注册此前是 stub,现已可用;MCP 注册走
//! `register_mcp_tool`。

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;

use crate::spec::ToolSpec;
use crate::tool::{Tool, ToolContext};
use reflect_protocol::{PermissionMode, ToolError, ToolOutput};

/// Where a tool came from. v1.0.0-rc2 起三个 source 全部正常工作 ——
///
/// `Builtin` 在 binary 启动期注册,`Runtime` 由用户代码或 MCP server 注入,
/// `Plugin` 由 `PluginManager` 在 plugin install / load 时挂载,
/// `Mcp` 由 `McpConnectionManager` 在 MCP server start / reload 时挂载。
/// `unregister_source(ToolSource::Plugin)` 用于 unload 时一次性反注册
/// 某 plugin 的所有 tool。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolSource {
    /// 由 binary 自带(如 `bash`、`read`、`grep`)。
    Builtin,
    /// 运行时由用户代码注册(如 plugins、自定义工具)。
    Runtime,
    /// 通过 `PluginManager` 从 plugin 加载。
    Plugin,
    /// v1.3:Registered from an MCP server. 安全基线与 Plugin / Runtime
    /// 相同:不显式声明权限时默认 `Prompt`。
    Mcp,
}

impl Default for ToolSource {
    fn default() -> Self {
        ToolSource::Builtin
    }
}

impl ToolSource {
    /// v1.3:此 source 是否属于「外部 / 不可信」工具。外部工具默认走
    /// 最低权限(`Prompt` / 审批),除非显式声明更严(`Deny`)或显式
    /// 声明白名单(`Auto` —— 仅 builtin)。这是安全基线:外部工具
    /// 无法把主程序最低门禁降为 `Auto`。
    pub fn is_external(self) -> bool {
        matches!(
            self,
            ToolSource::Runtime | ToolSource::Plugin | ToolSource::Mcp
        )
    }

    /// v1.3:此 source 的最低权限 floor。外部工具 floor = `Prompt`;
    /// builtin floor = `Auto`(可显式提到更高等级)。该 floor 不被
    /// 工具自身 `required_permission()` 的返回值覆盖 —— 工具只能
    /// 把自己提到更严(例如 `Deny`),不能放宽。
    pub fn permission_floor(self) -> PermissionMode {
        if self.is_external() {
            PermissionMode::Prompt
        } else {
            PermissionMode::Auto
        }
    }
}

/// v1.3:把外部工具包一层,使其 `required_permission()` 不会低于
/// `ToolSource::permission_floor()`。这样 MCP / plugin / runtime 工具
/// 不能把主程序最低门禁降到 `Auto`,符合严格安全基线要求。
struct FloorEnforcingTool {
    inner: Arc<dyn Tool>,
    floor: PermissionMode,
}

#[async_trait]
impl Tool for FloorEnforcingTool {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn is_concurrency_safe(&self) -> bool {
        self.inner.is_concurrency_safe()
    }
    fn required_permission(&self) -> PermissionMode {
        // 取 inner 声明值与 floor 的较严者:`Prompt` 比 `Auto` 严,
        // `Deny` 永远最严。这里用序数比较 —— 直接 enum 大小比较
        // 不可用,所以用 `is_at_least` 显式枚举。
        let inner = self.inner.required_permission();
        if is_at_least(inner, self.floor) {
            inner
        } else {
            self.floor
        }
    }
    fn action_permission(&self, args: &serde_json::Value) -> PermissionMode {
        let inner = self.inner.action_permission(args);
        if is_at_least(inner, self.floor) {
            inner
        } else {
            self.floor
        }
    }
    async fn execute(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.inner.execute(ctx, args).await
    }
}

/// 权限"严格度"比较。`Prompt` / `Plan` / `AcceptEdits` / `Deny` 比 `Auto` 严;
/// `Bypass` / `Bubble` 比 `Auto` 严格程度不同 —— 这里只关心 floor 是 `Prompt`
/// 时是否会放宽,`Prompt` / `Deny` 都满足 >= Prompt;`Auto` / `Bypass` /
/// `Bubble` 都不满足(它们放行/自动通过,等同低于 Prompt 的门禁)。
/// `Plan` / `AcceptEdits` 按"至少 Prompt"算(它们会走 approval modal
/// 或 plan gate)。`Bypass` / `Bubble` 视为放行(满足 floor)。
fn is_at_least(mode: PermissionMode, floor: PermissionMode) -> bool {
    use PermissionMode::*;
    // 简化逻辑:floor = Prompt 时,mode 在
    // {Prompt, Plan, AcceptEdits, Deny, Bypass, Bubble} 都算"满足"。
    // floor = Auto 时任何非 Deny 都满足。
    match floor {
        Auto => !matches!(mode, Deny),
        Prompt => !matches!(mode, Auto),
        // 其他 floor(mode)不太可能被本函数调用,但保守按 mode >= floor 返 true。
        _ => true,
    }
}

/// 可用工具的线程安全注册表。
#[derive(Default)]
#[allow(clippy::type_complexity)] // pre-M5: simple type, complexity is acceptable
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, (ToolSource, Arc<dyn Tool>)>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个工具,source 为 `Builtin`(内建工具的默认值)。
    pub fn register(&self, tool: Arc<dyn Tool>) {
        self.register_with_source(ToolSource::Builtin, tool);
    }

    /// 用指定的 source 注册一个工具。
    ///
    /// v1.0.0-rc2 起 `ToolSource::Plugin` 不再 warn + fallthrough,而是
    /// 原样保留 source 标签 —— `PluginManager` 在 install / load 时通过
    /// 此 API 挂载 plugin 提供的能力,反注册走 `unregister(name)`(由
    /// `PluginManager::unload` 调用)。其他 source 行为保持原状。
    ///
    /// v1.3:此方法**不**强制外部工具安全 floor —— 调用方需用
    /// [`Self::register_with_source_floor`] / [`Self::register_runtime_tool`]
    /// / [`Self::register_plugin_tool`] / [`Self::register_mcp_tool`]
    /// 等便捷方法,才会触发 `FloorEnforcingTool` 包装。
    /// 老代码(before v1.3)暂保持原行为不变,boot 路径会切到新 API。
    pub fn register_with_source(&self, source: ToolSource, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.write().insert(name, (source, tool));
    }

    /// v1.3:带安全 floor 的注册入口。外部 source(Runtime / Plugin /
    /// Mcp)→ `FloorEnforcingTool` 包装,`required_permission()` /
    /// `action_permission()` 不会低于 `source.permission_floor()`。
    /// Builtin → 直通,保留原始声明。
    pub fn register_with_source_floor(&self, source: ToolSource, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        let floor = source.permission_floor();
        let wrapped: Arc<dyn Tool> = if source.is_external() && floor == PermissionMode::Prompt {
            Arc::new(FloorEnforcingTool { inner: tool, floor })
        } else {
            tool
        };
        self.tools.write().insert(name, (source, wrapped));
    }

    /// 注册一个 plugin 提供的 tool —— `PluginSource::Plugin` source 的便捷方法。
    ///
    /// v1.3:走 `register_with_source_floor`,自动应用 plugin 源的安全 floor
    /// (`Prompt`)。plugin 工具自身 `required_permission()` 声明若为
    /// `Auto` 会被提升为 `Prompt`(用户不能被 plugin 静默放行)。
    /// 通常 `PluginManager` 通过此 API 把 plugin manifest 里声明的能力
    /// 挂到主程序,卸载时用 `unregister(name)` 反注册。
    pub fn register_plugin_tool(&self, tool: Arc<dyn Tool>) {
        self.register_with_source_floor(ToolSource::Plugin, tool);
    }

    /// v1.3:注册一个 runtime 提供的 tool(`load_skill` / `note` 等
    /// 内置用户代码工具)。走 `register_with_source_floor`,应用
    /// runtime 源的 `Prompt` floor。
    pub fn register_runtime_tool(&self, tool: Arc<dyn Tool>) {
        self.register_with_source_floor(ToolSource::Runtime, tool);
    }

    /// v1.3:注册一个 MCP server 提供的 tool。MCP 工具可能来自任意
    /// 第三方代码,即使单个 tool 声明 `Auto`,主程序安全下限仍是
    /// `Prompt`。MCP server 启动 / reload 路径应走此 API(原
    /// `register_with_source(Mcp, _)` 不再触发 floor)。
    pub fn register_mcp_tool(&self, tool: Arc<dyn Tool>) {
        self.register_with_source_floor(ToolSource::Mcp, tool);
    }

    /// 列出当前所有 tool 的 (name, source) 对。
    ///
    /// 主要给 `PluginManager` 用 —— 它需要知道哪些 tool 来自 plugin,
    /// 才能在 unload 时按 source tag 反注册(而非依赖 name 推测)。
    /// 顺序按 name 字典序,与 `list()` 一致。
    pub fn list_with_source(&self) -> Vec<(String, ToolSource)> {
        let mut out: Vec<_> = self
            .tools
            .read()
            .iter()
            .map(|(name, (source, _))| (name.clone(), *source))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// 反注册某个 source 下的所有 tool,返回实际移除的数量。
    ///
    /// `PluginManager::unload` 调用此方法一次清空该 plugin 的所有 tool;
    /// 也可用于测试清理。
    pub fn unregister_source(&self, source: ToolSource) -> usize {
        let mut map = self.tools.write();
        let before = map.len();
        map.retain(|_, (s, _)| *s != source);
        before - map.len()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.read().get(name).map(|(_, t)| t.clone())
    }

    /// 按名字反注册一个工具。返回 `true` 表示确实移除了一个工具。
    /// 给 v0.3 MCP 集成用 —— 在 server 关闭或重启时反注册工具
    /// (`McpConnectionManager::reload` 的 `on_remove` 回调)。
    ///
    /// 内部锁不向外暴露 —— caller 不需要关心三个 source 的存储细节。
    pub fn unregister(&self, name: &str) -> bool {
        self.tools.write().remove(name).is_some()
    }

    /// 仅在没有同名工具注册的情况下才注册。返回 `true` 表示成功插入,
    /// `false` 表示同名工具已存在(调用方应 `warn!` 并跳过)。
    ///
    /// v0.3 MCP 加载走此路径:`mcp__<server>__<tool>` 前缀保证与 7 个
    /// Reflect 内置 tool 不重名,但如果用户配置出 `mcp__fs__bash` 与
    /// builtin `bash` 仍然能并存(MCP 走 `mcp__` 前缀隔离)。如出现
    /// 真冲突(同名 tool 重复 register),返回 `false` 提示 caller warn。
    pub fn register_if_absent(&self, source: ToolSource, tool: Arc<dyn Tool>) -> bool {
        let mut map = self.tools.write();
        let name = tool.name().to_string();
        if map.contains_key(&name) {
            return false;
        }
        map.insert(name, (source, tool));
        true
    }

    /// v1.3:`register_if_absent` + 安全 floor 的组合。外部 source
    /// (Runtime / Plugin / Mcp) → `FloorEnforcingTool` 包装后再插入;
    /// builtin → 直通。返回 `true` 表示成功插入,`false` 表示同名
    /// 已存在(跳过,warn 由调用方决定)。
    pub fn register_if_absent_with_floor(&self, source: ToolSource, tool: Arc<dyn Tool>) -> bool {
        let mut map = self.tools.write();
        let name = tool.name().to_string();
        if map.contains_key(&name) {
            return false;
        }
        let floor = source.permission_floor();
        let wrapped: Arc<dyn Tool> = if source.is_external() && floor == PermissionMode::Prompt {
            Arc::new(FloorEnforcingTool { inner: tool, floor })
        } else {
            tool
        };
        map.insert(name, (source, wrapped));
        true
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<_> = self.tools.read().keys().cloned().collect();
        names.sort();
        names
    }

    /// v1.1.0 Phase 4:批量注册工具,排除 `excluded` 列表里的工具名。
    ///
    /// 用法:`reflect_task::coordinator::build_worker_tool_registry`
    /// 把父 registry 的 builtin + runtime 工具复制到 worker registry,
    /// 同时排除 `INTERNAL_WORKER_TOOLS`(`TeamCreate` / `TeamDelete`
    /// / `send_message`)。返回成功注册的工具数。
    ///
    /// 排除优先级高于同名覆盖:即使 caller 在 `tools` 里传了一个
    /// `TeamCreate`,worker registry 也不会拿到它(coordinator 拥有
    /// team 生命周期)。
    pub fn register_except(
        &self,
        source: ToolSource,
        tools: Vec<Arc<dyn Tool>>,
        excluded: &[&str],
    ) -> usize {
        let mut count = 0usize;
        for tool in tools {
            let name = tool.name();
            if excluded.contains(&name) {
                tracing::debug!(tool = %name, "register_except: 跳过 excluded tool");
                continue;
            }
            self.register_with_source(source, tool);
            count += 1;
        }
        count
    }

    /// 为每个已注册工具构建一个 `ToolSpec`(M2 把这些传给 `ChatRequest.tools`)。
    pub fn list_specs(&self) -> Vec<ToolSpec> {
        self.tools
            .read()
            .values()
            .map(|(_, t)| ToolSpec::Function {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters_schema(),
                required_permission: t.required_permission(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolContext, ToolError};
    use async_trait::async_trait;
    use reflect_protocol::ToolOutput as Out;

    struct T;
    #[async_trait]
    impl Tool for T {
        fn name(&self) -> &str {
            "t"
        }
        fn description(&self) -> &str {
            "d"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        async fn execute(&self, _: ToolContext, _: serde_json::Value) -> Result<Out, ToolError> {
            unreachable!()
        }
    }

    #[test]
    fn register_get_list_specs() {
        let r = ToolRegistry::default();
        r.register(Arc::new(T));
        assert!(r.get("t").is_some());
        assert!(r.get("missing").is_none());
        assert_eq!(r.list(), vec!["t".to_string()]);
        let specs = r.list_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name(), "t");
    }

    /// v0.3: MCP server 关闭时反注册 tool。
    #[test]
    fn unregister_removes_tool_by_name() {
        let r = ToolRegistry::default();
        r.register(Arc::new(T));
        assert!(r.get("t").is_some());
        assert!(r.unregister("t"));
        assert!(r.get("t").is_none());
        // 第二次 unregister 返回 false,语义幂等。
        assert!(!r.unregister("t"));
        assert!(!r.unregister("never_existed"));
    }

    /// v0.3: MCP 注册工具时同名冲突不覆盖,返回 false。
    #[test]
    fn register_if_absent_skips_on_collision() {
        let r = ToolRegistry::default();
        // 第一次注册 builtin 't' 成功。
        assert!(r.register_if_absent(ToolSource::Builtin, Arc::new(T)));
        // 第二次同源注册同名字 → 跳过。
        assert!(!r.register_if_absent(ToolSource::Runtime, Arc::new(T)));
        // builtin 't' 仍在。
        let t = r.get("t").expect("first register survives collision");
        assert_eq!(t.name(), "t");
    }

    /// v1.0.0-rc2: `ToolSource::Plugin` 不再 warn,保留 source tag。
    /// 这是 plugin 系统接入点 —— `PluginManager` 通过
    /// `register_plugin_tool` 挂载 plugin 能力。
    #[test]
    fn plugin_source_is_preserved_not_warned() {
        let r = ToolRegistry::default();
        r.register_plugin_tool(Arc::new(T));
        let pairs = r.list_with_source();
        assert_eq!(pairs, vec![("t".to_string(), ToolSource::Plugin)]);
    }

    /// `register_with_source(Plugin, _)` 与 `register_plugin_tool` 等价。
    #[test]
    fn register_with_source_plugin_equals_helper() {
        let r = ToolRegistry::default();
        r.register_with_source(ToolSource::Plugin, Arc::new(T));
        let (_, src) = r
            .list_with_source()
            .into_iter()
            .find(|(n, _)| n == "t")
            .unwrap();
        assert_eq!(src, ToolSource::Plugin);
    }

    /// `unregister_source` 只移除指定 source 的 tool,其他 source 不动。
    #[test]
    fn unregister_source_filters_by_tag() {
        let r = ToolRegistry::default();
        r.register_with_source(ToolSource::Builtin, Arc::new(T));
        struct U;
        #[async_trait]
        impl Tool for U {
            fn name(&self) -> &str {
                "u"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }
        r.register_plugin_tool(Arc::new(U));
        let removed = r.unregister_source(ToolSource::Plugin);
        assert_eq!(removed, 1);
        // builtin 't' 保留,plugin 'u' 消失。
        assert!(r.get("t").is_some());
        assert!(r.get("u").is_none());
    }

    // ── v1.1.0 Phase 4:排除式注册(register_except)──────────────

    /// 批量注册,排除名单外的工具成功;名单内跳过 + 计数不变。
    #[test]
    fn register_except_skips_excluded() {
        struct A;
        #[async_trait]
        impl Tool for A {
            fn name(&self) -> &str {
                "alpha"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }
        struct B;
        #[async_trait]
        impl Tool for B {
            fn name(&self) -> &str {
                "beta"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }
        let r = ToolRegistry::default();
        let n = r.register_except(
            ToolSource::Builtin,
            vec![Arc::new(A), Arc::new(B)],
            &["beta"],
        );
        assert_eq!(n, 1, "beta 应被排除");
        assert!(r.get("alpha").is_some());
        assert!(r.get("beta").is_none(), "beta 不应注册");
    }

    /// 空 excluded 列表 = 全部注册(等价于遍历 register_with_source)。
    #[test]
    fn register_except_with_empty_excluded_registers_all() {
        struct A;
        #[async_trait]
        impl Tool for A {
            fn name(&self) -> &str {
                "alpha"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }
        struct B;
        #[async_trait]
        impl Tool for B {
            fn name(&self) -> &str {
                "beta"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }
        let r = ToolRegistry::default();
        let n = r.register_except(ToolSource::Runtime, vec![Arc::new(A), Arc::new(B)], &[]);
        assert_eq!(n, 2);
    }

    // ── v1.3:外部工具最低权限 floor ─────────────────────────────────

    /// 自定义测试工具:声明 `required_permission = Auto`,模拟
    /// "外部 tool 试图把主程序最低门禁降为 Auto"。
    struct AutoTool;
    #[async_trait]
    impl Tool for AutoTool {
        fn name(&self) -> &str {
            "auto-tool"
        }
        fn description(&self) -> &str {
            "tries to be Auto"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        fn is_concurrency_safe(&self) -> bool {
            true
        }
        fn required_permission(&self) -> PermissionMode {
            PermissionMode::Auto
        }
        async fn execute(&self, _: ToolContext, _: serde_json::Value) -> Result<Out, ToolError> {
            unreachable!()
        }
    }

    /// v1.3 不变式:`is_external()` 对 Runtime / Plugin / Mcp 返回 true,
    /// Builtin 返回 false。安全 floor 由这个判断驱动。
    #[test]
    fn external_source_classification() {
        assert!(!ToolSource::Builtin.is_external());
        assert!(ToolSource::Runtime.is_external());
        assert!(ToolSource::Plugin.is_external());
        assert!(ToolSource::Mcp.is_external());
    }

    /// v1.3 不变式:`permission_floor()` 对外部 source 返回 Prompt,
    /// builtin 返回 Auto。这构成安全基线。
    #[test]
    fn permission_floor_per_source() {
        assert_eq!(ToolSource::Builtin.permission_floor(), PermissionMode::Auto);
        assert_eq!(
            ToolSource::Runtime.permission_floor(),
            PermissionMode::Prompt
        );
        assert_eq!(
            ToolSource::Plugin.permission_floor(),
            PermissionMode::Prompt
        );
        assert_eq!(ToolSource::Mcp.permission_floor(), PermissionMode::Prompt);
    }

    /// v1.3 不变式:`register_with_source_floor(Runtime, AutoTool)` 必须
    /// 抬高权限到 Prompt。外部 tool 不能把主程序最低门禁降为 Auto。
    #[test]
    fn floor_promotes_external_auto_to_prompt() {
        let r = ToolRegistry::default();
        r.register_with_source_floor(ToolSource::Runtime, Arc::new(AutoTool));
        let tool = r.get("auto-tool").expect("tool registered");
        assert_eq!(
            tool.required_permission(),
            PermissionMode::Prompt,
            "Runtime source 必须 floor 到 Prompt"
        );
    }

    /// v1.3 不变式:`register_plugin_tool` / `register_mcp_tool` /
    /// `register_runtime_tool` 都触发 floor 包装。Plugin / Mcp 同样
    /// 不能把 Auto 工具降级。
    #[test]
    fn floor_helpers_promote_external_auto_to_prompt() {
        struct McpTool;
        #[async_trait]
        impl Tool for McpTool {
            fn name(&self) -> &str {
                "mcp-tool"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            fn required_permission(&self) -> PermissionMode {
                PermissionMode::Auto
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }
        struct PluginTool;
        #[async_trait]
        impl Tool for PluginTool {
            fn name(&self) -> &str {
                "plugin-tool"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            fn required_permission(&self) -> PermissionMode {
                PermissionMode::Auto
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }

        let r = ToolRegistry::default();
        r.register_plugin_tool(Arc::new(PluginTool));
        r.register_mcp_tool(Arc::new(McpTool));

        let plugin = r.get("plugin-tool").expect("plugin tool registered");
        assert_eq!(
            plugin.required_permission(),
            PermissionMode::Prompt,
            "Plugin source 必须 floor 到 Prompt"
        );
        let mcp = r.get("mcp-tool").expect("mcp tool registered");
        assert_eq!(
            mcp.required_permission(),
            PermissionMode::Prompt,
            "Mcp source 必须 floor 到 Prompt"
        );
    }

    /// v1.3 不变式:Builtin tool 走 `register_with_source_floor` 不应
    /// 被包装(避免对 builtin 工具行为造成意外)。Builtin floor 是
    /// Auto,所以 `register_with_source_floor(Builtin, AutoTool)` 应
    /// 保持原 Auto。
    #[test]
    fn builtin_source_keeps_original_permission() {
        struct BuiltinAuto;
        #[async_trait]
        impl Tool for BuiltinAuto {
            fn name(&self) -> &str {
                "builtin-auto"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            fn required_permission(&self) -> PermissionMode {
                PermissionMode::Auto
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }

        let r = ToolRegistry::default();
        r.register_with_source_floor(ToolSource::Builtin, Arc::new(BuiltinAuto));
        let tool = r.get("builtin-auto").expect("registered");
        assert_eq!(
            tool.required_permission(),
            PermissionMode::Auto,
            "Builtin 不应被 floor 包装"
        );
    }

    /// v1.3 不变式:`register_if_absent_with_floor` 对外部 source 同样
    /// 应用 floor,且在 name 冲突时返回 false 不覆盖。
    #[test]
    fn register_if_absent_with_floor_skips_collisions() {
        let r = ToolRegistry::default();
        // 先注册一个外部 Auto tool → floor 抬到 Prompt。
        assert!(r.register_if_absent_with_floor(ToolSource::Runtime, Arc::new(AutoTool)));
        // 同名再注册一个 → 返回 false,旧 tool 保留。
        struct AutoTool3;
        #[async_trait]
        impl Tool for AutoTool3 {
            fn name(&self) -> &str {
                "auto-tool"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            fn required_permission(&self) -> PermissionMode {
                PermissionMode::Auto
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }
        assert!(!r.register_if_absent_with_floor(ToolSource::Mcp, Arc::new(AutoTool3)));
        let tool = r.get("auto-tool").expect("registered");
        // 第一个注册是 Runtime,被 floor 抬到 Prompt。
        assert_eq!(tool.required_permission(), PermissionMode::Prompt);
    }

    /// v1.3 不变式:外部 tool 即使显式声明 `Deny`,floor 不放宽 —
    /// `FloorEnforcingTool` 保留 tool 自己的更严声明。`Deny` 比
    /// `Prompt` 严,所以应保持 Deny。
    #[test]
    fn floor_keeps_stricter_declarations() {
        struct StrictExternalTool;
        #[async_trait]
        impl Tool for StrictExternalTool {
            fn name(&self) -> &str {
                "strict-external"
            }
            fn description(&self) -> &str {
                "d"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type":"object"})
            }
            fn is_concurrency_safe(&self) -> bool {
                true
            }
            fn required_permission(&self) -> PermissionMode {
                PermissionMode::Deny
            }
            async fn execute(
                &self,
                _: ToolContext,
                _: serde_json::Value,
            ) -> Result<Out, ToolError> {
                unreachable!()
            }
        }
        let r = ToolRegistry::default();
        r.register_with_source_floor(ToolSource::Mcp, Arc::new(StrictExternalTool));
        let tool = r.get("strict-external").expect("registered");
        // Deny 比 Prompt 严 → tool 自己的声明保留。
        assert_eq!(tool.required_permission(), PermissionMode::Deny);
    }
}
