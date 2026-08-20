# Reflect

**English** | [简体中文](README.md)

> An AI agent runtime **framework** written in Rust: a 33-crate workspace
> (6-layer architecture), a 4-node StateGraph engine, 23 built-in tools,
> 8 hook events, multiple LLM providers, MCP / LSP integration, and JSONL
> rollout persistence.
>
> This repository is a **pure framework layer**: the primary interfaces are
> the `reflect` library facade (Builder + 60+ re-exports) and the Rust /
> Python / TypeScript integration entry points; `reflect-cli` is just a thin
> headless entry produced alongside it (a single `reflect` binary, including
> the `serve` subcommand used by the SDKs).
>
> Script ecosystems (Python / TS) can embed the framework directly via
> `reflect serve` + the official SDKs: spawn the binary, speak the JSONL
> stdio protocol, and **register local functions as custom tools the LLM
> can call** — the Rust engine handles inference and orchestration while
> business logic stays in the host language.

**0.0.1** · 33 crates · **2200+ tests** · Apache-2.0

## Features

| Capability | Implementation |
|------------|----------------|
| 4-node StateGraph | PreLoop → ModelCall → ToolExec → CheckStop self-loop |
| OpenAI + Anthropic + Ollama | SSE streaming, prompt caching, extended thinking, local NDJSON |
| 23 built-in tools | bash / read / write / edit / grep / glob / web_fetch / web_search / notebook_edit / image_view / task tools + Plan-mode control plane, etc. |
| Cross-language custom tools | Clients (Python / TS) register local functions as LLM tools; the core dispatches execution requests and receives results back |
| 8 hook events × 7 decisions | PreToolUse / PostToolUse / PostToolUseFailure / Stop / SessionStart + 3 task-lifecycle events |
| 4-tier context compaction | microcompact → smart_prune → LLM summarize (escalation) |
| Subagents | Tool-per-Agent, nesting depth ≤ 3 |
| Multi-agent orchestration | discussion (sequential/concurrent), task / team, DAG pipelines, goal mode |
| Python / TypeScript SDKs | Pure stdlib / zero native dependencies; spawn `reflect serve` over the JSONL stdio protocol |
| Persistence | JSONL rollout + resume + session index + LLM trace recording |
| MCP | stdio / streamable-http transports + tool prefix + reload diff |
| LSP | LSP server configuration management and integration |
| Plugins | install / enable / disable / uninstall + marketplace |
| Permissions & sandbox | permissions rule engine + multi-level sandbox |
| Plan mode | `reflect exec --plan-mode` + `EnterPlanMode` / `ExitPlanMode` tools |
| Configuration | unified TOML + notify hot-reload + CLI subcommands |
| CLI | single `reflect` binary + 17 subcommands (incl. `serve`) |
| Built-in mock provider | `REFLECT_MODEL=mock` + scripted replies; examples / e2e / SDK tests run fully offline |
| Testing | 2200+ tests · wiremock · insta snapshots · cross-platform CI |

## Quick Start

```bash
# 1. Build (requires Rust 1.85+, pinned by rust-toolchain.toml)
git clone https://cnb.cool/Demon1019/Reflect-Agent && cd Reflect-Agent
cargo build --release            # whole workspace (libs + binary); `make build` builds only the CLI

# 2. Set an API key (pick one of three)
export OPENAI_API_KEY=sk-...
# or
export ANTHROPIC_API_KEY=sk-ant-...
# or persist it to the config file
./target/release/reflect login --provider anthropic --api-key sk-ant-...

# 3. Rust library integration — the framework's primary interface (one-line Builder start)
cargo run -p reflect --example headless_run -- "say hi"
# More examples: custom_tool / multi_turn / custom_provider / hook_listener / discussion_demo

# 4. Headless one-shot conversation (thin CLI entry; JSONL Event stream to stdout, logs to stderr)
./target/release/reflect exec "what is 2+2?" | jq -c '.msg.type'

# 5. Python / TypeScript embedding — spawn `reflect serve` over the JSONL stdio protocol
export REFLECT_BIN=./target/release/reflect     # how the SDK locates the binary (or install it on PATH)
python3 - <<'EOF'
import sys; sys.path.insert(0, "sdks/python")
from reflect import ReflectAgent
agent = ReflectAgent.spawn()          # returns once the handshake (session_configured) completes
print(agent.prompt("hello"))          # aggregates deltas into the final text
agent.close()
EOF

# 6. Continue the previous session
./target/release/reflect exec -c

# 7. Plan mode — read-only research mode
./target/release/reflect exec --plan-mode "summarize the auth module"
```

Offline development: examples and e2e scripts use `REFLECT_MODEL=mock` to skip
real LLM network calls.

## Architecture

### Workspace layers (dependencies flow top-down)

| Layer | Crates | Responsibility |
|-------|--------|----------------|
| `protocol/` | reflect-protocol | Submission / Op / EventMsg / Item + RolloutRecorder trait. The common contract for all layers |
| `abilities/` | llm, tools, hooks, skills, memory, agent-def, prompt, compact, recovery, notes, sanitize | Capability primitives: ModelClient trait + providers, Tool trait + built-in tools, HookEngine, compaction escalation, etc. |
| `resources/` | config, permissions, plugin, rollout, telemetry, sandbox, ast | Config hot-reload, permission rules, plugin lifecycle, JSONL persistence, tree-sitter code search |
| `orchestration/` | subagent, discussion, task, pipeline, goal | Multi-agent orchestration: subagent factory (depth ≤ 3), discussion orchestration, tasks/teams, DAG pipelines, goal mode |
| `integrations/` | mcp, lsp, integration, stream | MCP client (stdio/streamable-http), LSP, streaming session backend |
| `runtime/` | core, exec, cli, reflect, py | AgentThread + StateGraph, headless, CLI entry, lib facade (60+ re-exports + Builder), PyO3 bindings |

### Core data flow

- `AgentThread` (reflect-core) consumes `Submission`s (mpsc channel);
  `submission_loop` drives the 4-node `StateGraph`:
  **PreLoop → ModelCall → (ToolExec → PreLoop)\* → CheckStop**.
  When the model stops issuing tool calls, CheckStop ends the turn
  (a Stop hook may veto and force the turn to continue).
- `NodeContext` carries per-turn dependencies: model registry, RoutingPolicy,
  HookEngine, ToolExecutionQueue, event channel, CancellationToken,
  approval gates.
- Clients (TUI / exec / serve / lib) all communicate through the same
  Submission/Event protocol; in headless mode Events stream to stdout as
  JSONL while tracing logs go to stderr (`reflect exec | jq` stays clean);
  `reflect serve` reuses the same protocol as a resident service that the
  SDKs wrap (remote tool requests/responses, approvals, etc. are all
  protocol-level Op / EventMsg variants).

### Key design decisions

- **Protocol v0 is frozen**: adding Op/EventMsg variants is non-breaking; existing variants never change
- **JSONL to stdout, logs to stderr**: `reflect exec | jq` is never corrupted
- **ToolError lives in reflect-protocol**: breaks the tools ↔ hooks dependency cycle
- **StateGraph via enum + match**: fixed 4-node structure, no petgraph
- **Error handling**: `thiserror` for library crates, `anyhow` for application layers (exec/cli)
- **serve is an embedding entry, not a dev assistant**: built-in hooks are disabled by default (exec / TUI keep "unconfigured = all enabled"), so Stop hooks like `verification` never run tests in the host's cwd

## CLI Subcommands

| Subcommand | Purpose |
|------------|---------|
| `reflect exec` | One-shot headless run, JSONL Event stream to stdout (supports `-c` / `-r N` / `--resume <uuid>` / `--agent <name>` / `--plan-mode`) |
| `reflect serve` | Resident stdio JSONL session service (protocol entry for the Python / TS SDKs; one Submission per stdin line, one Event per stdout line; supports the same resume flags) |
| `reflect discussion` | Multi-agent discussion orchestration (`run -c <toml>` / `ls`) |
| `reflect login` | Write provider credentials to `~/.reflect/config.toml` |
| `reflect mcp` | MCP server configuration (`ls` / `add` / `remove` / `test` / `show`) |
| `reflect config` | Config read/write (`show` / `set` / `unset` / `edit` / `ls`) |
| `reflect session` | Persistent session management (`ls` / `show` / `rm` / `fork` / `rename` / `export`) |
| `reflect traces` | Browse local LLM call records (`ls` / `show`; data in `~/.reflect/traces/model-io/`) |
| `reflect doctor` | Best-effort environment self-check (supports `--check-network`) |
| `reflect plugin` | Plugin install / list / enable / disable / uninstall + marketplace |
| `reflect lsp` | LSP server configuration management |
| `reflect task` | Structured tasks + teams (TaskCreate / TaskList / TeamCreate, etc.) |
| `reflect pipeline` | DAG pipelines (team-plan → team-prd → team-exec → team-verify) |
| `reflect security` | Security scan (cargo audit) |
| `reflect workspace` | git clone / sync for workspaces |
| `reflect update` | Print upgrade notes (currently offline) |
| `reflect version` | Print the version |

### Resume flags

`-c` / `-r N` / `--resume <uuid>` are mutually exclusive (a clap group):

```bash
reflect exec -c "continue the previous work"    # resume the most recent session
reflect exec -r 2 "follow up on the second one" # by index
reflect exec --resume <uuid>                    # by UUID
```

### Typical workflows

```bash
# First-time setup
reflect login --provider anthropic --api-key sk-ant-...
reflect exec "hello"

# Attach an MCP filesystem server
reflect mcp add filesystem --command npx \
                          --args "-y" \
                          --args "@modelcontextprotocol/server-filesystem" \
                          --args "$HOME"
reflect mcp test filesystem

# Switch models
reflect config set anthropic.model claude-3-haiku-20240307
# ConfigWatcher hot-reloads automatically

# Troubleshooting
reflect doctor --check-network
RUST_LOG=debug reflect exec "hello" 2>debug.log

# Continue a session
reflect session ls
reflect exec -c "continue where we left off"
```

## Plan Mode

Plan mode produces a complete plan before the agent runs write tools
(`bash` / `write` / `edit`): the `PlanModeGate` hook blanket-denies write
tools (read-only tools like read / grep / glob remain available), and after
finishing research the agent calls `ExitPlanMode` to present the plan and
leave Plan mode.

```bash
reflect exec --plan-mode "refactor X"   # headless research (read-only)
```

## Multilingual SDKs (Python / TypeScript)

Script projects can drive the entire framework without writing any Rust:
`reflect serve` provides a resident stdio JSONL session (one process =
one resident AgentThread, in-memory state shared across turns, resume
supported), and the official SDKs wrap it with idiomatic APIs for each
language — **inference and orchestration run in the Rust engine, business
logic stays in the host language**.

The standout capability is **cross-language custom tool registration**:
declare a Python / TS function as a tool (JSON Schema parameters); when
the LLM calls it, the core dispatches a `tool_execution_request`, the SDK
executes the handler locally and returns the result — tool implementations
never enter the Rust process.

**Python** (`sdks/python`, pure stdlib with zero dependencies; binary
lookup: `REFLECT_BIN` env → `reflect` on PATH):

```python
from reflect import ReflectAgent, ToolOutput, ContentBlock

agent = ReflectAgent.spawn()

def get_weather(args):
    return ToolOutput(
        content=[ContentBlock(type="text", text=f"{args['city']}: sunny")],
        is_error=False, metadata={}, elapsed_ms=0,
    )

# local function -> a tool the LLM can call
agent.register_tool("get_weather", "Get weather for a city",
                    {"type": "object",
                     "properties": {"city": {"type": "string"}},
                     "required": ["city"]},
                    get_weather)

for ev in agent.submit("What's the weather in Beijing?"):  # blocking iterator, incremental events
    if ev["msg"]["type"] == "agent_message_delta":
        print(ev["msg"]["delta"], end="", flush=True)
    if ev["msg"]["type"] == "turn_complete":
        break
agent.close()
```

**TypeScript** (`sdks/typescript`, pure TS with zero runtime dependencies;
isomorphic API):

```ts
import { ReflectAgent } from 'reflect-agent';

const agent = await ReflectAgent.spawn();
await agent.registerTool(
  'get_weather', 'Get weather for a city',
  { type: 'object', properties: { city: { type: 'string' } }, required: ['city'] },
  (args) => ({
    content: [{ type: 'text', text: `${args.city}: sunny` }],
    is_error: false, metadata: {}, elapsed_ms: 0,
  }),
);

const text = await agent.prompt("What's the weather in Beijing?");  // aggregates deltas
await agent.close();
```

Advanced surface of both SDKs: `submit()` returns an event stream filtered
by submission id (concurrent turns don't interfere), `interrupt()` aborts,
`approve()` answers approval requests; unknown protocol event types
degrade gracefully — adding variants to the framework never breaks the
SDKs.

- Wire protocol spec: [`sdks/PROTOCOL.md`](sdks/PROTOCOL.md) (handshake /
  tool execution flow / approval replies / shutdown semantics)
- Per-language details: [`sdks/python/README.md`](sdks/python/README.md) ·
  [`sdks/typescript/README.md`](sdks/typescript/README.md)
- Offline debugging: `REFLECT_MODEL=mock` (+ `REFLECT_MOCK_SCRIPT` for
  scripted replies) needs no API key; `./scripts/sdk_smoke.sh` runs both
  SDKs end-to-end against the real binary

## Examples

| Example | Demonstrates | Command |
|---------|--------------|---------|
| `headless_run` | Minimal one-shot prompt | `cargo run -p reflect --example headless_run -- "..."` |
| `custom_tool` | Custom Tool implementation | `cargo run -p reflect --example custom_tool` |
| `multi_turn` | Multiple turns on one agent with history | `cargo run -p reflect --example multi_turn` |
| `custom_provider` | Offline MockLlmClient | `cargo run -p reflect --example custom_provider` |
| `hook_listener` | TokenUsage + DangerousCommand hooks | `cargo run -p reflect --example hook_listener` |
| `discussion_demo` | 3-agent discussion demo | `cargo run -p reflect --example discussion_demo` |

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `OPENAI_API_KEY` | one of | OpenAI provider |
| `ANTHROPIC_API_KEY` | one of | Anthropic provider |
| `OLLAMA_HOST` | no | Ollama `base_url` (full URL incl. scheme) |
| `OLLAMA_API_KEY` | no | Ollama Cloud / reverse-proxy Bearer |
| `REFLECT_PROVIDER` | no | Force a provider (overrides TOML `[active]`) |
| `REFLECT_MODEL` | no | Override the default model spec; `=mock` runs examples / e2e / SDKs offline |
| `REFLECT_MOCK_SCRIPT` | no | Mock provider script (JSONL, one line per model call: text / tool_call) |
| `REFLECT_REMOTE_TOOL_TIMEOUT_SECS` | no | serve-mode timeout waiting for remote tool replies (default 120s) |
| `REFLECT_BIN` | no | Where SDKs locate the `reflect` binary (default: PATH) |
| `REFLECT_HOME` | no | Data directory override (default `~/.reflect`) |
| `REFLECT_AUTO_COMPACT_INPUT_TOKENS` | no | Compaction threshold (default 10,000) |
| `REFLECT_MAX_ITERATIONS` | no | Graph execution safety valve (max iterations) |
| `REFLECT_SANDBOX_*` | no | Sandbox policy (`STRICT` / `OS_LEVEL` / `WRITABLE`) |
| `RUST_LOG` | no | tracing filter (default `warn,reflect=info`) |

## Project Structure

```
crates/
├── protocol/               common contract
│   └── reflect-protocol        Submission / Op / EventMsg + RolloutRecorder
├── abilities/              capability primitives
│   ├── reflect-llm             ModelClient trait + OpenAI/Anthropic/Ollama + pricing
│   ├── reflect-tools           Tool trait + 23 built-in tools + ToolExecutionQueue
│   ├── reflect-hooks           HookEngine + 8 events + 7 decisions
│   ├── reflect-skills          SKILL.md scanning + loading
│   ├── reflect-memory          3-scope memory (Project/User/Session)
│   ├── reflect-agent-def       Markdown frontmatter agent definitions
│   ├── reflect-prompt          layered prompts + cache_control injection
│   ├── reflect-compact         4-tier compaction strategy escalation
│   ├── reflect-recovery        failure recovery
│   ├── reflect-notes           notes
│   └── reflect-sanitize        output sanitization
├── resources/              config / permissions / persistence
│   ├── reflect-config          unified TOML config + hot reload
│   ├── reflect-permissions     permission rule engine
│   ├── reflect-plugin          plugin lifecycle
│   ├── reflect-rollout         JSONL persistence + session index
│   ├── reflect-telemetry       observability
│   ├── reflect-sandbox         multi-level sandbox
│   └── reflect-ast             tree-sitter code search
├── orchestration/          multi-agent orchestration
│   ├── reflect-subagent        Tool-per-Agent subagent factory (depth ≤ 3)
│   ├── reflect-discussion      multi-agent discussion orchestration
│   ├── reflect-task            structured tasks / teams
│   ├── reflect-pipeline        DAG pipelines
│   └── reflect-goal            goal mode
├── integrations/           external protocol integrations
│   ├── reflect-mcp             MCP client (stdio / streamable-http)
│   ├── reflect-lsp             LSP integration
│   ├── reflect-integration     integration glue layer
│   └── reflect-stream          streaming session backend
└── runtime/                runtime and entry points
    ├── reflect-core            AgentThread + 4-node StateGraph
    ├── reflect-exec            headless + JSONL stdout + resume + serve
    ├── reflect-cli             top-level CLI routing (single reflect binary)
    ├── reflect                 lib facade (60+ re-exports + Builder)
    └── reflect-py              PyO3 Python bindings (skeleton)
sdks/                          multilingual SDKs
├── PROTOCOL.md                    serve wire protocol spec
├── python/                        pure-stdlib Python SDK (reflect-agent)
└── typescript/                    pure-TS npm SDK (reflect-agent)
```

The `reflect` binary is produced by the `reflect-cli` crate and is this
repository's **thin headless entry**; framework consumers' primary
interface is the `reflect` library facade (Builder / re-exports).

## Documentation

| Document | Contents |
|----------|----------|
| [`AGENTS.md`](AGENTS.md) | Development guide: commands, architecture layers, project conventions |
| [`sdks/PROTOCOL.md`](sdks/PROTOCOL.md) | serve wire protocol spec (shared implementation basis for the SDKs) |
| [`sdks/python/README.md`](sdks/python/README.md) | Python SDK usage and installation |
| [`sdks/typescript/README.md`](sdks/typescript/README.md) | TypeScript SDK usage and installation |
| [`SECURITY.md`](SECURITY.md) | Security policy and vulnerability reporting |

## Development

```bash
make check                                # cargo check --workspace
make check-fast C=reflect-core            # fast check for a single crate
cargo test --workspace                    # 2200+ tests
make test-fast                            # cargo nextest (must be installed)
make test-changed                         # run only tests affected by changes since HEAD
cargo clippy --workspace --all-targets -- -D warnings   # strict lint
cargo fmt --all                           # formatting
./scripts/e2e.sh                          # 15-step e2e smoke (build + examples + headless + serve + SDK + test + clippy)
./scripts/sdk_smoke.sh                    # offline smoke of both SDKs against the real binary (build release first)
./scripts/bench.sh                        # performance baselines (cold start / first event / RSS)
./scripts/discussion_smoke.sh             # discussion orchestration smoke
(cd sdks/typescript && npm test)          # TS SDK tests (Node mock serve, offline)
(cd sdks/python && python3 -m pytest)     # Python SDK tests (Node mock serve, offline)
make dist                                 # package DMG / tar.gz / zip into dist/
make gc                                   # clean intermediate artifacts cargo won't GC from target/
```

## Roadmap

Future plans include MCP OAuth, PostgreSQL session storage, IDE plugins,
npm per-platform binary packages and PyPI publishing (the SDKs are
currently used directly from the `sdks/` source directory), and more.

## License

Apache-2.0
