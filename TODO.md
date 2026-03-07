# TODO

## Technical Debt & Improvements

### 1. `run.rs` is a God Module (~700 lines)

**Problem:** `run.rs` handles service lifecycle (start/stop/restart), log rotation, PID management, runtime assembly, session worker coordination, factory traits, agent memory inspection, and the main event loop. This makes it hard to navigate, test in isolation, and modify without risk.

**Improvement:**
- Extract `RotatingLogWriter` and log rotation into a `logging.rs` module.
- Extract `SessionWorkerCoordinator` into a `session.rs` or `coordinator.rs` module.
- Extract `ProviderFactory`, `MemoryFactory`, `ChannelFactory`, and `RuntimeDependencies` into a `composition.rs` module.
- Extract PID/daemon management (`start_service`, `stop_service`, `read_pid`, `is_process_running`) into a `daemon.rs` module.
- Keep `run_service` as a thin orchestrator that composes these pieces.

### 2. `AgentRuntime::new` Takes 15 Parameters

**Problem:** The constructor is suppressed with `#[allow(clippy::too_many_arguments)]`. This signals a struct that has accumulated responsibilities without refactoring.

**Improvement:**
- Introduce a builder pattern or a `AgentRuntimeConfig` struct that groups related parameters (e.g., `provider + provider_kind + dispatcher`, `memory + summon_memories`, `workspace_root + app_base_dir`).
- Consider whether `summon_agents` and `summon_memories` belong on the runtime or should be resolved at call time through a service.

### 3. `react::run_loop` Also Has Too Many Parameters

**Problem:** `run_loop` takes 9 parameters including the provider, dispatcher, tool context, active tools, system prompt, agent ID, session ID, history, and max steps. Several of these could be grouped.

**Improvement:**
- Create a `ReactLoopContext` struct that bundles provider, dispatcher, tool_ctx, active_tools, and system_prompt.
- Pass agent_id and session_id as part of the context or a separate identity struct.

### 4. Monomorphic Provider Implementation

**Problem:** Only `GeminiProvider` exists. The `Provider` trait is well-designed but the runtime hardcodes `ProviderKind::Gemini` as the sole variant. Adding a new provider requires changes in `config.rs`, `lib.rs` (known_providers/known_models), and `run.rs` (factory match).

**Improvement:**
- Consider a provider registry pattern (similar to `ToolRegistry`) to make provider addition more mechanical.
- At minimum, document the exact touchpoints needed to add a new provider.

### 5. Error Type Abuse: `FrameworkError::Config` as Catch-All

**Problem:** `FrameworkError::Config(String)` is used for database pool failures, embedder failures, channel closures, and many other runtime errors that aren't configuration issues. This makes error categorization unreliable for callers.

**Improvement:**
- Add dedicated variants: `FrameworkError::Pool(String)`, `FrameworkError::Embedding(String)`, `FrameworkError::Channel(String)`.
- Reserve `Config` for actual configuration/validation errors.
- This enables better error handling, retry logic, and user-facing error messages.

### 6. Tool Argument Parsing is Fragile and Duplicated

**Problem:** `common.rs` has 7 different `parse_*_args` functions that each manually extract JSON fields with fallback chains (`value.get("key") -> value.as_str() -> trim_matches`). This pattern is repeated for every tool and isn't validated against the declared `input_schema_json`.

**Improvement:**
- Use `serde::Deserialize` with `#[serde(default)]` for typed argument structs (already done for `ExecArgs` and `ProcessArgs`, but not for memory, summon, task, memorize, forget, etc.).
- Consider a shared `parse_or_fallback<T: Deserialize>` helper.
- Validate args against the schema or at least rely on serde for structural validation.

### 7. Sandbox Architecture Gaps (Status Updated)

**Findings and status:**
- Dispatch-layer no-op in `execute_tool_with_sandbox`: fixed by enforcing `Tool::sandbox_aware()` when `SandboxMode::On`.
- WASM artifacts missing during development runs: fixed by resolving artifacts from env/install prefix and cargo target wasm outputs.
- Read/edit/path helper code triplication: fixed by extracting shared logic into `sandbox/common`.
- WASM `/tmp` mount used host shared temp dir: fixed with per-execution isolated temp mount and cleanup.
- Network tools bypass sandbox (`web_search`, `web_fetch`): known limitation, out of scope for this PR.
- Podman workspace mount is always read-write: known limitation, out of scope for this PR.

### 8. Blocking Embedder Under Async Mutex

**Problem:** `MemoryStore` wraps `TextEmbedding` in `Arc<Mutex<TextEmbedding>>` (tokio async mutex) but `fastembed`'s `embed()` is a blocking CPU-intensive operation. This blocks the tokio runtime thread during embedding computation.

**Improvement:**
- Use `tokio::task::spawn_blocking` for embedding calls (similar to how `run_wasm_guest` handles blocking work).
- Or use a `std::sync::Mutex` with `spawn_blocking` instead of an async mutex.

### 9. No Retry Logic for Provider Calls

**Problem:** If `Provider.generate()` fails (network timeout, rate limit, transient error), the ReAct loop immediately returns an error. There's no retry with backoff.

**Improvement:**
- Add configurable retry with exponential backoff for transient provider errors.
- Distinguish between retryable errors (timeouts, 429, 500+) and permanent errors (auth, bad request).
- Keep max_retries low (2-3) to avoid runaway costs.

### 10. Discord Channel Reconnection is Naive

**Problem:** In `channel.rs`, the Discord client reconnection loop (`loop { client.start_autosharded() }`) has a fixed 5-second delay on failure with no backoff or jitter. The `Channel::listen()` retry in `gateway.rs` has a fixed 1-second delay.

**Improvement:**
- Implement exponential backoff with jitter for reconnection.
- Add circuit breaker logic to avoid hammering Discord during extended outages.
- Log reconnection attempts with attempt count.

### 11. `ProcessManager` Uses Polling for Completion Detection

**Problem:** `spawn_completion_watcher` polls every 500ms to check if a background process has completed. For podman containers, this involves running `podman inspect` every 500ms.

**Improvement:**
- For host processes, use `child.wait()` in a spawned task instead of polling `try_wait()`.
- For podman containers, consider `podman wait` which blocks until completion.
- Fall back to polling only when the efficient approach isn't available.

### 12. Memory Pre-injection Calls `config.normalized()` Twice

**Problem:** In `query_preinject_hits`, `config.normalized()` is called, and then `rank_preinject_hits` internally calls it again on the same config. This is wasteful and could lead to subtle bugs if normalization isn't idempotent.

**Improvement:**
- Normalize once at the entry point and pass the normalized config through.

### 13. No Concurrent Tool Execution

**Problem:** In `dispatch.rs`, `execute_tool_calls` runs tools sequentially in a loop. When a provider requests multiple independent tool calls, they could run concurrently.

**Improvement:**
- Use `futures::future::join_all` or `tokio::JoinSet` for independent tool calls.
- Some tools may have ordering dependencies (e.g., read then edit), so this should be opt-in or heuristic-based.

### 14. `ToolCtx` Has Too Many Optional Fields

**Problem:** `ToolCtx` contains `Option<Arc<dyn SummonService>>`, `Option<Arc<dyn TaskService>>`, `Option<mpsc::Sender<InboundMessage>>`, and `Option<CompletionRoute>`. These are `None` in many contexts (summon targets, task workers), making the struct a grab-bag.

**Improvement:**
- Split into a base `ToolCtx` with always-present fields and an `AgentToolCtx` that adds agent-specific capabilities.
- Or use a capability-based approach where tools query for services they need.

### 15. `channel.rs` Mixes Transport and Policy Logic

**Problem:** `channel.rs` contains both the `Channel` trait/implementations and the Discord inbound policy engine (`classify_inbound`, `InboundDecision`, policy resolution). These are separate concerns.

**Improvement:**
- Extract Discord inbound policy into `discord_policy.rs` or `inbound_policy.rs`.
- Keep `channel.rs` focused on the transport abstraction.

### 16. No Graceful Shutdown

**Problem:** `run_service` runs `loop { gateway.next_message() }` with no signal handling. The `stop_service` function sends SIGTERM via `kill()`, but the running service doesn't handle it gracefully -- it doesn't flush pending work, close database connections cleanly, or wait for in-progress agent executions.

**Improvement:**
- Add `tokio::signal::ctrl_c()` or SIGTERM handler.
- Implement graceful drain: stop accepting new messages, wait for in-progress sessions to complete (with timeout), then shut down.

### 17. `config.rs` Validation and Schema Could Be Split

**Problem:** `config.rs` is over 800 lines mixing struct definitions, deserialization, secret resolution, path normalization, validation, and Discord policy resolution. It's the second-largest file.

**Improvement:**
- Extract validation functions into `config_validation.rs`.
- Extract Discord inbound policy resolution (`DiscordInboundConfig::resolve`) into the policy module.
- Keep `config.rs` focused on struct definitions and loading.

---

## Trait-First Architecture For Extreme Testability

### Goals
- Make core runtime components plug-and-play via traits.
- Enable deterministic, zero-network, zero-external-dependency integration tests.
- Minimize concrete-type coupling in orchestration code.

### Phase 1: Memory Abstraction
- Introduce `MemoryBackend` trait that captures required behavior now provided by `MemoryStore`.
- Move methods used by runtime/tools into the trait surface first:
  - `append_message`
  - `recent_messages`
  - `query_preinject_hits`
  - `memorize`
  - `semantic_forget_long_term`
  - `list_long_term_facts`
- Provide production implementation:
  - `SqliteMemoryBackend` (with embedder).
- Provide test implementation:
  - `NoEmbedderSqliteMemoryBackend` (semantic paths return explicit unsupported/tooling error).
- Replace direct concrete `MemoryStore` usage in `AgentRuntime`, `ToolCtx`, and runtime assembly with trait object (`Arc<dyn MemoryBackend>`) or generic where justified.
- Add compatibility shim so existing callers can migrate incrementally.

### Phase 2: Provider Abstraction Cleanup
- Keep `Provider` trait as the model interface.
- Add provider adapter traits for orchestration:
  - `ProviderFactory` (already present) should be stabilized and moved to a dedicated module.
- Add deterministic test doubles:
  - scripted sequence provider (multiple steps/calls),
  - failure-injection provider (timeouts, malformed responses, empty responses).
- Ensure no production code path requires real network in tests.

### Phase 3: Channel / Gateway Abstraction
- Keep `Channel` trait as transport contract.
- Add reusable test channels:
  - capture channel (collect outbound + typing events),
  - scripted inbound channel (queue-based),
  - flaky channel (inject intermittent send/listen failures).
- Move retry/supervision policy behind an injectable strategy so gateway failure behavior is testable.

### Phase 4: Runtime Composition Root
- Create a dedicated composition module (e.g. `composition/`):
  - centralizes dependency wiring (provider, memory, channels, tools, prompt loader).
- Define a `RuntimeContainer` struct composed only of trait-based dependencies.
- Remove implicit construction from `run_service`; make `run_service` consume container builder defaults.
- Keep CLI path unchanged while enabling test-specific container assembly.

### Phase 5: Tooling and Side-Effect Boundaries
- Introduce trait wrappers for side-effectful services:
  - clock/time provider,
  - filesystem adapter for sensitive operations,
  - process execution adapter,
  - web/search adapter.
- Route built-in tools through these interfaces so tool tests do not require real system/network behavior.
- Add failure-injection harnesses for each adapter.

### Phase 6: Prompt / Config / Path Loading
- Abstract disk/config access behind traits:
  - prompt source,
  - config source,
  - path resolver.
- Add in-memory implementations for integration tests to avoid temp-file boilerplate when not needed.

### Phase 7: Test Suite Expansion (After Refactor)
- Add deterministic e2e scenarios:
  - successful invoke flow,
  - unknown-agent route,
  - passive-context message path,
  - provider failure with safe error reply,
  - channel send failure and retry/log behavior.
- Add contract tests per trait implementation to ensure behavior parity between prod and test backends.

### Design Rules
- Prefer small cohesive traits over large "god interfaces".
- Keep orchestration code depending on traits, not concrete implementations.
- Make every external side effect injectable.
- Default implementations should remain production-safe and backward compatible.
