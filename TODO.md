# TODO

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
- Prefer small cohesive traits over large “god interfaces”.
- Keep orchestration code depending on traits, not concrete implementations.
- Make every external side effect injectable.
- Default implementations should remain production-safe and backward compatible.
