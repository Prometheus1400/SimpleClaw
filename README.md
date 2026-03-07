# SimpleClaw

A lightweight, multi-agent agentic framework built in Rust. SimpleClaw connects AI agents to messaging platforms (Discord, logging) with local SQLite-backed memory, WASM-sandboxed tool execution, and a modular prompt composition system.

## Architecture Overview

```
                         +-------------------+
                         |       CLI         |
                         | (clap, tracing)   |
                         +---------+---------+
                                   |
                         +---------v---------+
                         |   LoadedConfig    |
                         | (config.yaml +    |
                         |  secrets.yaml)    |
                         +---------+---------+
                                   |
               +-------------------v-------------------+
               |          Runtime Assembly             |
               |  (run.rs: assemble_runtime_state)     |
               +---+----------+----------+----------+--+
                   |          |          |          |
          +--------v--+  +---v----+  +--v---+  +--v---------+
          | Provider  |  |Gateway |  |Memory|  |ToolRegistry|
          | (Gemini)  |  |        |  |Store |  |            |
          +--------+--+  +---+----+  +--+---+  +--+---------+
                   |          |         |          |
                   |   +------v------+  |          |
                   |   |  Channels   |  |          |
                   |   | +--------+  |  |          |
                   |   | |Discord |  |  |          |
                   |   | +--------+  |  |          |
                   |   | |Logging |  |  |          |
                   |   | +--------+  |  |          |
                   |   +------+------+  |          |
                   |          |         |          |
               +---v----------v---------v----------v---+
               |           AgentRuntime                |
               | (per-agent: config, prompt, memory,   |
               |  tools, skills, process manager)      |
               +-------------------+-------------------+
                                   |
                         +---------v---------+
                         |   ReAct Loop      |
                         | (react::run_loop) |
                         +---------+---------+
                                   |
                    +--------------+--------------+
                    |              |              |
             +------v---+  +------v---+  +------v---+
             |Tool Calls|  |  Final   |  |  Empty   |
             |(dispatch)|  | Response |  |(continue)|
             +------+---+  +----------+  +----------+
                    |
         +----------v-----------+
         |   Tool Dispatcher    |
         | +------------------+ |
         | | NativeDispatcher | | <-- Provider supports function calling
         | +------------------+ |
         | | XmlDispatcher    | | <-- Fallback XML-based tool protocol
         | +------------------+ |
         +----------+-----------+
                    |
    +---------------+---------------+
    |       |       |       |       |
  +--v-+ +--v-+ +--v-+ +--v-+  +--v-+
  |exec| |read| |edit| |mem | |fetch|
  +----+ +----+ +----+ +----+ +-----+
  (12 builtin tools + dynamic skill tools)
```

## Data Flow: Message Lifecycle

```
  User sends message (Discord/Logging)
          |
          v
  Channel.listen() -> InboundMessage
          |
          v
  Gateway.next_message() (mpsc aggregation)
          |
          v
  SessionWorkerCoordinator.dispatch(session_key, msg)
          |
          +-- per-session unbounded channel
          |   (sequential processing per session,
          |    concurrent across sessions)
          v
  handle_inbound_once(state, inbound)
          |
          +-- !invoke? -> record_context() (passive observation)
          |
          +-- invoke? -> AgentRuntime.run()
                |
                +-- append user message to short-term memory
                +-- load seeded history (recent_messages)
                +-- build turn system prompt (+ memory pre-injection)
                +-- inject caller context (user identity)
                +-- resolve active tools (registry + skills)
                |
                v
          react::run_loop (up to max_steps iterations)
                |
                +-- Provider.generate(system_prompt, history, tools)
                +-- ToolDispatcher.parse_response()
                |     |
                |     +-- ToolCalls -> execute each -> append to history -> continue
                |     +-- FinalResponse -> return text
                |     +-- Empty -> continue
                |
                v
          append assistant reply to short-term memory
                |
                v
          Gateway.send_message() -> Channel.send_message()
```

## Module Architecture

### Core Modules

| Module | File | Responsibility |
|--------|------|----------------|
| **lib** | `src/lib.rs` | Crate root, CLI dispatch, tracing init |
| **cli** | `src/cli.rs` | Argument parsing (clap): system, logs, status, providers, models, agent memory |
| **config** | `src/config.rs` | Two-tier YAML config (global `~/.simpleclaw/config.yaml` + per-agent `agent.yaml`), validation, secret resolution |
| **paths** | `src/paths.rs` | `~/.simpleclaw/` directory layout resolution |
| **secrets** | `src/secrets.rs` | Secret resolution: `${secret:<name>}` references resolved from env vars then `secrets.yaml` |
| **error** | `src/error.rs` | `FrameworkError` enum (Io, Yaml, Db, Provider, Tool, Config) via `thiserror` |

### Runtime & Orchestration

| Module | File | Responsibility |
|--------|------|----------------|
| **run** | `src/run.rs` | Service lifecycle (start/stop/restart as daemon), runtime assembly, session worker coordinator, log rotation, agent memory inspection |
| **agent** | `src/agent.rs` | `AgentRuntime` struct -- per-agent execution context. Owns provider, memory, tools, prompt, skill tools. Orchestrates `RuntimeSummonService` and `RuntimeTaskService` |
| **gateway** | `src/gateway.rs` | Multi-channel message aggregation via mpsc. Routes inbound messages and dispatches outbound replies |
| **channel** | `src/channel.rs` | `Channel` trait + implementations: `DiscordChannel` (serenity), `LoggingChannel` (dev/test). Discord inbound policy (per-server, per-channel, DM rules, mention requirements) |
| **react** | `src/react.rs` | Core ReAct loop: iterates provider calls and tool executions up to `max_steps`. Log sanitization and secret redaction |
| **dispatch** | `src/dispatch.rs` | `ToolDispatcher` trait with two strategies: `NativeDispatcher` (provider-native function calling) and `XmlDispatcher` (XML-in-text fallback). Owner-restricted tool authorization |

### Memory & Prompt

| Module | File | Responsibility |
|--------|------|----------------|
| **memory** | `src/memory.rs` | Dual-database SQLite memory: short-term (sessions/messages) and long-term (facts with embeddings). Uses `fastembed` for local embeddings, `sqlite-vec` for vector similarity. Supports memorize (with dedup + supersede), semantic query, semantic forget |
| **prompt** | `src/prompt.rs` | Layered prompt assembly from markdown files: `IDENTITY.md`, `AGENT.md`, `USER.md`, `MEMORY.md`, `SOUL.md` |

### Tools

| Module | File | Responsibility |
|--------|------|----------------|
| **tools** | `src/tools/mod.rs` | `Tool` trait, `ToolRegistry`, `ActiveTools`, `ToolCtx` (execution context), `ProcessManager` (background process lifecycle with host + podman backends) |
| **tools/builtin** | `src/tools/builtin/` | 12 builtin tools: `memory`, `memorize`, `forget`, `summon`, `task`, `exec`, `process`, `read`, `edit`, `web_search`, `web_fetch`, `clock` |
| **tools/sandbox** | `src/tools/sandbox.rs` | WASM sandbox via `wasmtime` + WASI preview 1. Runs `read_tool` and `edit_tool` as sandboxed WASM guests with workspace and tmp mounts |
| **tools/skill** | `src/tools/skill.rs` | Dynamic skill tools loaded from `skills/<id>/SKILL.md` files (agent-scoped or global-scoped). Exposed as callable tools that return raw markdown |
| **tools/builtin/common** | `src/tools/builtin/common.rs` | Shared argument parsing for tools (exec, memory, summon, task, memorize, forget, process) |

### Testing

| Module | File | Responsibility |
|--------|------|----------------|
| **testing** | `src/testing.rs` | Public e2e test harness: `run_single_gateway_roundtrip()` with mock provider, capture channel, ephemeral SQLite databases. Exercises full runtime assembly through `handle_inbound_once` |

## Key Design Decisions

### Dual Tool Dispatch Strategy
Providers that support native function calling (e.g., Gemini) use `NativeDispatcher` which passes tool definitions as structured specs. For providers that don't, `XmlDispatcher` injects tool definitions into the system prompt and parses `<tool_call>` XML from response text.

### Dual-Database Memory Architecture
Short-term memory (session messages) and long-term memory (semantic facts with embeddings) are stored in separate SQLite databases. Long-term facts use `sqlite-vec` for cosine similarity search. The `memorize` tool deduplicates within a time window and supersedes semantically similar facts (>= 0.92 similarity).

### Memory Pre-injection
On each turn, the system optionally queries long-term memory with the user's message, ranks hits by a weighted similarity score (factoring in importance), and injects relevant context into the system prompt before the LLM call.

### WASM Sandboxing
The `read` and `edit` tools can run as WASM guests via `wasmtime` with WASI preview 1, providing filesystem isolation through preopened directory mounts. The sandbox mode is configured per-agent.

### Process Management
The `exec` tool supports both foreground and background execution. Background processes can run on the host or in podman containers (when sandbox mode is on). A completion watcher monitors background processes and re-injects completion events back into the gateway.

### Session Worker Coordination
Inbound messages are dispatched to per-session workers via `SessionWorkerCoordinator`. Messages within the same session are processed sequentially (preserving conversation order), while different sessions execute concurrently. Workers idle-timeout after 5 minutes.

### Owner-Restricted Tools
Sensitive tools (`exec`, `process`, `forget`, `summon`, `edit`, `memorize`) are restricted to configured owner user IDs, preventing unauthorized users from executing destructive operations.

## Configuration

### Global Config (`~/.simpleclaw/config.yaml`)

Defines provider, runtime, gateway, agents, discord, database, and embedding settings.

### Per-Agent Config (`<workspace>/agent.yaml`)

Defines per-agent model override, sandbox mode, enabled tools, and enabled skills.

### Prompt Layers (`<workspace>/`)

| File | Purpose |
|------|---------|
| `IDENTITY.md` | Who the agent is (persona, name, role) |
| `AGENT.md` | How the agent operates (instructions, guidelines) |
| `USER.md` | Context about the primary user |
| `MEMORY.md` | Persistent goals or static long-term context |
| `SOUL.md` | Deep personality, ethical boundaries, "vibe" |

### Secrets (`~/.simpleclaw/secrets.yaml`)

Secrets are referenced via `${secret:<name>}` syntax in config. Resolved from environment variables first, then the secrets file.

## Common Commands

```bash
cargo build                    # Build the project
cargo run                      # Run the service (foreground)
cargo test                     # Run tests
cargo fmt --all                # Format all code
simpleclaw system start        # Start as background daemon
simpleclaw system stop         # Stop the daemon
simpleclaw status              # Show service status
simpleclaw logs --follow       # Tail service logs
simpleclaw providers list      # List available providers
simpleclaw models list         # List available models
simpleclaw agent memory \
  --agent <id> --memory long \
  --limit 20                   # Inspect agent memory
```

## WASM Artifacts

Build the sandboxed tool WASM guests:

```bash
cargo build --package read_tool --package edit_tool --target wasm32-wasip1 --release
```

## Install / Uninstall

```bash
./scripts/install.sh           # Install binary + WASM assets to ~/.cargo
./scripts/install.sh --debug   # Install debug build
./scripts/uninstall.sh         # Remove installed artifacts
./scripts/uninstall.sh --stop  # Stop service and uninstall
```
