# Lightweight Rust Agentic Framework (LRAF): Project Plan

## 1. Architecture Overview
A lightweight, multi-agent framework built in Rust. It features a decoupled **Gateway Layer** for gathering context across various platforms and uses local SQLite (`sqlite-vec`) for memory. 

The architecture is highly modular and asynchronous:
1. **Stateless Configuration:** Two-tiered YAML loaded on boot. A root `config.yaml` for global settings and isolated `<workspace>/agent.yaml` files.
2. **OpenClaw Prompt Architecture:** Agent personas are defined via a modular Markdown stack: `IDENTITY.md` (who), `AGENT.md` (how), `USER.md` (context), `MEMORY.md` (history), and `SOUL.md` (vibe/ethics).
3. **ZeroClaw-Inspired Traits:** Intelligence and messaging are abstracted behind `Provider` and `Channel` traits. 
4. **Agentic RAG:** Agents use a `memory` tool to query the local vector database only when needed. Embeddings are handled entirely locally to eliminate API costs.
5. **Robust Error Handling:** Uses `thiserror` for library-level enums and `color_eyre` for application-layer reporting.



---

## 2. Implementation Phases

### Phase 1: Foundation, CLI, & Prompt Composition
**Goal:** Parse CLI arguments, load workspaces, and compile the "Mega-Prompt."

* **CLI & Errors:** Initialize `color_eyre` and `clap` parser.
* **Agent Registry:** The loader will scan `<workspace>/` for:
    * `agent.yaml`: Technical config (tools, model selection, routing).
    * `IDENTITY.md`: Persona, name, and role.
    * `AGENT.md`: Operational logic (ReAct instructions, tool-use guidelines).
    * `USER.md`: Fixed knowledge about the primary user/target.
    * `MEMORY.md`: Persistent goals or long-term context.
    * `SOUL.md`: Deep personality traits, ethical boundaries, and "vibe."
* **Prompt Assembler:** Logic to concatenate these files into a single, high-context System Prompt on boot.

### Phase 2: The Channel Abstraction & Gateway Layer
**Goal:** Decouple the messaging platform using a `Channel` trait.

* **The `Channel` Trait:** Define `send_message`, `broadcast_typing`, and `listen`.
* **Discord Implementation:** Build `DiscordChannel` wrapping `serenity`. 
* **Gateway Pipeline:** Verify IDs, log passively via `mpsc`, and dispatch tasks to agents with an injected `Arc<dyn Channel>`.

### Phase 3: Database & Memory Layer
**Goal:** Set up high-concurrency local SQLite with vector extensions.

* **Schema:** `sessions`, `messages`, and `vec_memory` (with embedding BLOBs).
* **Connection Tuning:** Use `deadpool-sqlite` with `WAL` mode and `busy_timeout=5000`.
* **Background Worker:** A dedicated task to batch writes and generate local embeddings (e.g., `all-MiniLM-L6-v2`) for every incoming message.

### Phase 4: The Provider Abstraction
**Goal:** Decouple the AI generation provider.

* **Internal Types:** Define `ToolCall`, `Message`, and `ProviderResponse`.
* **The `Provider` Trait:** `async fn generate(&self, system_prompt, history, tools)`.
* **Implementation:** Build `GeminiProvider`.

### Phase 5: Free-Form Agents & The ReAct Loop
**Goal:** Build the execution loop powered by the OpenClaw prompt stack and system tools.

* **The Toolset:**
    * **`memory`**: Semantic query of `sqlite-vec` (local RAG).
    * **`summon`**: Handoff to another agent with a summary.
    * **`search`**: Web search via DuckDuckGo.
    * **`clock`**: Current ISO-8601 timestamp.
    * **`fetch`**: Scrapes URL content into markdown.
    * **`read`**: Reads local files (sandboxed).
* **Core ReAct Loop:**
    1. **System Prompt Injection:** Start with the compiled Mega-Prompt from Phase 1.
    2. **Generation:** Call the `Provider`.
    3. **Action:** Execute `Tool`s. Append observations to the scratchpad and loop.
    4. **Safety:** Exit if `max_steps` is exceeded.
    5. **Finalize:** Reply via `Channel` and log results.

### Phase 6: Polish & Future-Proofing
* **Observability:** Structured logging with `tracing`.
* **Security:** Draft architecture for future Tool Sandboxing.
