# AGENTS.md

## Commit Style
- Use Conventional Commits for all commit messages.
- Format: `<type>(<scope>): <description>`.
- Example: `fix(config): expand workspace path handling`.

## Project Evolution Policy
- Do not add migrations or backward-compatibility handling by default.
- Implement the current schema/behavior directly unless the user explicitly asks for migration/back-compat support.

## Planning Expectations
- When presenting a plan, include the reasoning behind the approach.
- Plans must also include a preview of the proposed code changes so the intended edits are clear before implementation.

## Code Spiritual Guidelines
1. Clean code with strong single-responsibility boundaries.
2. Make invalid states irrepresentable.
3. Tend toward dependency injection for testability, composability, and extensibility.
4. Break up complicated modules into focused submodules.
5. Prefer clear ownership semantics: reduce unnecessary `Arc`, `Mutex`, `Arc<Mutex<_>>`, and cloning where applicable.
6. Avoid double-pointer indirection such as `&Arc<T>` unless there is a specific need.
7. Prefer explicit interfaces at module boundaries; hide concrete types behind traits/type aliases where it improves substitution.
8. Prefer composition over inheritance-style abstractions; keep abstractions shallow and purposeful.
9. Require observability for important flows: structured logs, clear error context, and stable event names.
10. Do not normalize code smells with blanket suppressions such as `#[allow(unused)]`, `#[allow(dead_code)]`, or similar exceptions unless they are genuinely necessary and narrowly scoped with a clear reason.
