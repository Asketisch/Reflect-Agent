# ZhuYe

ZhuYe is an agent harness you embed in your own product. One Rust engine can be linked in-process, driven for a single turn with `zhuye exec`, or kept alive as a stdio JSONL session with `zhuye serve` and the [Python](sdks/python/zhuye/agent.py) / [TypeScript](sdks/typescript/src/agent.ts) SDKs. The `zhuye` binary in this repository has no TUI subcommand. A comment in the workspace [Cargo.toml](Cargo.toml) points the interactive UI at [Reflect-TUI](https://cnb.cool/Demon1019/Reflect-CLI). That comment is not a runtime dispatch.

Use Claude Code, Codex, OpenCode, or Pi when you want to sit in a terminal and write code. Use ZhuYe when you want that engine inside your own service, script, or interface.

中文：[README.md](README.md).

## Where it differs from the four

The loop is the same shape everywhere: read context, call a model, run tools, decide whether to stop. The difference is what you are buying.

| Choice | Claude Code | Codex | OpenCode | Pi | ZhuYe |
| --- | --- | --- | --- | --- | --- |
| Shape | A coding assistant you sit inside. The prompt is written for Claude | A coding assistant you sit inside. Commands go into an OS sandbox by default, and the models lean OpenAI | An open terminal product. Swap providers. The client and the resident server are separate | A tiny loop with four built-in tools. Anything else is a TypeScript extension you add | An engine. Link the library, or speak stdio JSONL to `zhuye serve`. This binary has no TUI subcommand |
| Changing models | A different model still gets the Claude prompt | A different model still gets the OpenAI prompt | A different endpoint, still one agent prompt | One very short prompt for every model | Built-in prompts stack as `default`, then vendor, then exact model name. The main turn has a fallback chain. Compaction uses the compact primary only |
| Multi-agent | Open another child session | Open another child session | Open another child session | Open another child session from an extension | Discussion, a DAG pipeline, and goal mode are engine primitives. The main loop stays as it is |
| Long context | Compact once the session grows | Compact once the session grows | Compact once the session grows | Almost no scaffolding around the loop | Truncate locally first, and call a summarizer only if that is still over budget. Re-read active files after a non-noop compact. Inject the subagent registry and session notes every turn |
| Isolation | A kernel sandbox, with network allowed per domain | A kernel sandbox. Network is off by default while the workspace is writable | Permissions are confirmation prompts. Isolation means you bring a container | No approval prompts and no sandbox unless you add them | `bash` enters an OS sandbox by default. If the backend is missing, the call fails instead of falling back to a raw shell. macOS denies network by default. Linux restricts writes |

The next sections are the evidence for the ZhuYe column.

### Swap the prompt with the model, then the model with the role

The prompt library stacks three layers of built-in copy: `default`, then a vendor (claude, gpt, gemini, deepseek, kimi, qwen, grok, glm, minimax), then the exact model name. `.zhuye/prompts/` in the workspace overrides `~/.zhuye/prompts/`. A file only needs the sections you want to change; missing sections fall back to the embedded default. See [`library.rs`](crates/abilities/zhuye-prompt/src/library.rs).

Only the main turn walks `Role::Main`: primary, then `fallbacks`, with credential cooldown in between. Compaction copies the `Role::Compact` primary into the summarizer at construction and then fails over only inside that spec's credential pool. It does not read the compact slot's `fallbacks`. Nothing calls `Role::Subagent`. A subagent uses the parent model from startup, or a separate registry from `[subagent_providers]`. See [`RoutingPolicy`](crates/abilities/zhuye-llm/src/policy.rs).

### Discussion, pipeline, goal

A discussion runs the roles in a TOML file, either sequentially or concurrently. `consensus_window` is how many rounds must contain a consensus report from every participant. `max_rounds` is a hard stop. With `judge` on, an independent model issues a structured verdict each round, and only `agreed` counts; a participant's own Consensus message is just a signal. `judge` defaults to off, which keeps the self-report behavior. See [`DiscussionConfig`](crates/orchestration/zhuye-discussion/src/models.rs).

```bash
zhuye discussion run -c crates/orchestration/zhuye-discussion/examples/discussion.toml
```

With no provider configured, the discussion state machine still finishes and returns an empty transcript, which is enough to check the command. Real turns need a provider.

A pipeline is a declared DAG. Downstream of a failed node is `skipped` under both `abort` and `continue_collect`. Branches with no dependency on the failure still run. Neither policy breaks the loop early. The report status is `failed` or `partial`. The example in the repo is plan → prd → exec → verify, and each node names a team that already exists. See [`zhuye-pipeline`](crates/orchestration/zhuye-pipeline/src/lib.rs) and [`config.example.toml`](crates/orchestration/zhuye-pipeline/examples/config.example.toml).

```bash
zhuye pipeline run \
  -c crates/orchestration/zhuye-pipeline/examples/config.example.toml \
  --topic "build a CLI todo app"
```

Goal mode checks at the end of a turn. It does not add a node to the state graph. After the client submits `EnterGoalMode`, every turn ends with an LLM self-check, plus an optional command. Both have to pass. The same blocker has to show up 3 times in a row before the goal is marked blocked. The token budget defaults to 500000 and stops the run when it is spent. If the goal is still open, the continuation text is pushed onto the steering queue and merged into the next user input. The engine does not start another turn by itself. See [`GoalController::on_turn_end`](crates/orchestration/zhuye-goal/src/controller.rs); the merge is in [`submission_loop.rs`](crates/runtime/zhuye-core/src/submission_loop.rs). One line into `zhuye serve`:

```json
{"id":"1","op":{"type":"enter_goal_mode","goal":"tests pass","verify_command":"cargo test -p zhuye-core --lib","token_budget":500000}}
```

### Compact locally, then restore by trigger

The compactor estimates tokens and does nothing under the threshold. Past it, `microcompact` keeps the system message and the first user message, replaces old tool results with placeholders, and strips thinking. If that is not enough, `smart_prune` truncates by tool type and drops the oldest non-kept messages. Only then does it call a summarizer. That call uses the compact primary copied at construction, so it can be a different model from the main turn. See [`Compactor`](crates/abilities/zhuye-compact/src/strategy.rs).

Active files are re-read only after a non-noop compaction. The finished-subagent registry and session notes are injected on every `pre_loop`, then appended to the request in `model_call`. Notes are a FIFO of at most 30 entries. Appending one does not call a model, and the file is JSONL, separate from the long-form `MEMORY.md`. See [`zhuye-recovery`](crates/abilities/zhuye-recovery/src/lib.rs) and [`zhuye-notes`](crates/abilities/zhuye-notes/src/lib.rs).

### Sandbox, per platform

`bash` runs in an OS sandbox by default, with `strict` on. macOS uses Seatbelt, denies network by default, and can write the workspace. Linux uses Landlock and restricts writes. If the kernel cannot apply it, the command fails instead of falling back to a raw `sh -c`. There is no kernel rule that turns "network off" into a real deny on Linux, and the status line does not claim one. See [`os_sandbox.rs`](crates/resources/zhuye-sandbox/src/os_sandbox.rs).

## Three ways to try it

The engine requires Rust 1.85. The binary:

```bash
cargo build -p zhuye-cli
ZHUYE_MODEL=mock ./target/debug/zhuye exec "hello"
```

`ZHUYE_MODEL=mock` is an explicit offline model. A missing API key does not silently switch you onto it. For a real model, set `OPENAI_API_KEY` or `ANTHROPIC_API_KEY`, or configure a provider in `~/.zhuye/config.toml`. Events go to stdout. Logs go to stderr.

In-process (`mock` needs no key here either):

```rust
use zhuye::{Submission, Zhuye};

let agent = Zhuye::builder("mock").workspace(".").build()?;
let mut stream = agent.submit(Submission::user_input("hello")).await;
while let Some(event) = stream.next().await {
    let _ = event;
}
```

`build()` installs compaction, memory, skills, and session recording. See [`Zhuye::builder`](crates/runtime/zhuye/src/lib.rs).

Tools written in the host language stay in the host process. The SDK starts `zhuye serve` (`ZHUYE_BIN` selects the binary). `register_tool` hands the engine a name and a JSON Schema. When the model calls it, the engine emits `tool_execution_request`, the function runs in Python or TypeScript, and the result is sent back.

```python
from zhuye import ZhuyeAgent, ToolOutput, ContentBlock

agent = ZhuyeAgent.spawn()
agent.register_tool(
    "get_weather",
    "查询城市天气",
    {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    lambda args: ToolOutput(
        content=[ContentBlock(type="text", text=f"{args['city']} 晴")],
        is_error=False,
    ),
)
for ev in agent.submit("北京天气如何?"):
    if ev["msg"]["type"] == "turn_complete":
        break
agent.close()
```

The Python package is [`sdks/python`](sdks/python) (stdlib only). The TypeScript package is [`sdks/typescript`](sdks/typescript).

## CLI at a glance

The `zhuye` binary has 19 subcommands, grouped by purpose:

| Command | Purpose |
|---------|---------|
| `exec` | One-shot headless turn; events stream to stdout as JSONL |
| `serve` | Long-lived stdio JSONL session service; the SDK protocol entry |
| `discussion` | Run a multi-agent discussion from a TOML config |
| `pipeline` | Run a declarative DAG pipeline |
| `task` | Structured tasks and teams |
| `login` | Write provider credentials into `~/.zhuye/config.toml` |
| `models` | List / pick models currently available from the provider; fetched live from the API |
| `mcp` | Add, remove, and test MCP server configs |
| `lsp` | Inspect LSP server configs |
| `plugin` | Install / enable / disable / uninstall plugins |
| `config` | Inspect / edit `~/.zhuye/config.toml`; hot-reloaded on write |
| `session` | List / show / delete / fork persisted sessions; search across sessions |
| `traces` | Local model-io call records |
| `security` | `cargo audit` scan |
| `workspace` | git clone / sync of workspaces |
| `doctor` | Best-effort environment check (config / rollout / mcp / rustc) |
| `init` | Generate an AGENTS.md project brief |
| `update` | Print upgrade instructions (no network) |
| `version` | Print the version |

For flags, see `zhuye <command> --help`. To resume a session, use `zhuye exec --resume`, by ordinal (`-r 2`) or full UUID.

## Configuration notes

Configuration lives in `~/.zhuye/config.toml` (move it entirely with `ZHUYE_HOME`):

```toml
[active]
provider = "openai"
# Optional: trim the main agent's tool face at the config layer,
# allowlist ∩ / denylist −, same semantics as tools / disallowed_tools
# in .zhuye/agents/*.md:
# tools = ["read", "grep"]
# disallowed_tools = ["bash", "write", "web_search"]

[openai]
api_key = ""                                  # leave empty for keyless endpoints
base_url = "https://api.kilo.ai/api/gateway"  # already contains a version segment (/v1, /v4): /v1 is not appended again
model = "free"                                # free model: picked from the live catalog at runtime
temperature = 0.3                             # optional: default sampling temperature (explicit per-request values win)
max_tokens = 4096                             # optional: default output cap
```

`model = "free"` pairs with model discovery: `zhuye models list --free-only` shows the currently free models, `zhuye models pick --free-only` picks one. The catalog is fetched live from the provider API; model ids are not hardcoded. For live-model drills, see [`FREE_MODEL_PRACTICE.md`](docs/FREE_MODEL_PRACTICE.md).

`zhuye config set` writes are hot-reloaded by ConfigWatcher; no process restart is needed.

## Environment variables

| Variable | Purpose |
|----------|---------|
| `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` | Credentials for the matching provider; one is enough to start |
| `OLLAMA_HOST` / `OLLAMA_API_KEY` | Ollama base_url (full URL with scheme) / Bearer |
| `ZHUYE_PROVIDER` / `ZHUYE_MODEL` | Force the provider / override the model spec; `ZHUYE_MODEL` strips a known provider prefix (e.g. `openai/m1`), `=mock` runs offline |
| `ZHUYE_MOCK_SCRIPT` | Mock script: path to a JSONL file, one model call per line (text / tool_call / tool_call_delta) |
| `ZHUYE_HOME` | Data directory (default `~/.zhuye`); config, prompts, session notes — all state follows it |
| `ZHUYE_BIN` | Where the SDK looks for the `zhuye` binary (default: PATH) |
| `ZHUYE_REMOTE_TOOL_TIMEOUT_SECS` | serve remote-tool receipt timeout (default 120s) |
| `ZHUYE_AUTO_COMPACT_INPUT_TOKENS` | Compaction trigger threshold (default 10,000) |
| `ZHUYE_MAX_ITERATIONS` | Graph execution iteration cap |
| `ZHUYE_SANDBOX_STRICT` etc. | Sandbox level switches (`STRICT` / `OS_LEVEL` / `WRITABLE`) |
| `RUST_LOG` | tracing filter (default `warn,zhuye=info`) |

## Documentation

| Doc | Contents |
|-----|----------|
| [`docs/architecture.md`](docs/architecture.md) | Architecture: layer responsibilities, directory layout, core data flow, design decisions |
| [`docs/sdk.md`](docs/sdk.md) | SDK guide: serve mode, remote tool flow, offline testing |
| [`sdks/PROTOCOL.md`](sdks/PROTOCOL.md) | serve wire protocol spec |
| [`docs/prompts.md`](docs/prompts.md) | Prompt library and layered overrides |
| [`docs/tools-and-hooks.md`](docs/tools-and-hooks.md) | Built-in tools and hooks |
| [`docs/plugins.md`](docs/plugins.md) | Plugins: manifest, capabilities, marketplace |
| [`docs/PLAN_MODE.md`](docs/PLAN_MODE.md) | Plan mode |
| [`docs/sanitize.md`](docs/sanitize.md) | Output sanitization |
| [`docs/FREE_MODEL_PRACTICE.md`](docs/FREE_MODEL_PRACTICE.md) | Free-model live practice |
| [`docs/release.md`](docs/release.md) | SDK releases (PyPI / npm) |
| [`AGENTS.md`](AGENTS.md) | Development guide and project conventions |

## Development

```bash
make check                                            # cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./tests/run_all.sh functional                         # capability coverage layer (Python against the real binary)
./scripts/e2e.sh                                      # end-to-end smoke
```

Examples and e2e run offline via `ZHUYE_MODEL=mock`; no API key needed.

## Limits

The interactive UI is not in this repository. Terminal polish is Claude Code's tradeoff. Four tools and a very short prompt are Pi's. ZhuYe does not ship a product surface for either.

The Linux sandbox restricts writes. It does not cut the network. Only macOS Seatbelt denies network when `allow_network = false`, which is the default.

A parent session can have at most 16 subagents in flight at once. The constant is named `MAX_DEPTH`. It counts in-flight work: a finished child frees a slot. It is not a nesting depth and not a lifetime spawn count. See [`zhuye-subagent`](crates/orchestration/zhuye-subagent/src/lib.rs).

This page does not compare token use or task pass rates. The public harness benchmarks do not include ZhuYe.

## Also in the engine

MCP, LSP, hooks, permission modes, resuming a session by thread id (`zhuye exec --resume`), rewinding a conversation to a turn (`Op::Rewind`), and a local model-io record of each model call (`zhuye traces`) all have counterparts in the other four. The implementations are [`zhuye-mcp`](crates/integrations/zhuye-mcp), [`zhuye-lsp`](crates/integrations/zhuye-lsp), [`zhuye-hooks`](crates/abilities/zhuye-hooks), [`zhuye-permissions`](crates/resources/zhuye-permissions), [`zhuye-exec`](crates/runtime/zhuye-exec), [`Op`](crates/protocol/zhuye-protocol/src/op.rs), and [`zhuye-telemetry`](crates/resources/zhuye-telemetry).

## License

The engine and CLI are [Apache-2.0](LICENSE). The Python SDK and the TypeScript SDK declare MIT in their own package manifests. Repository: <https://github.com/Asketisch/ZhuYe>.
