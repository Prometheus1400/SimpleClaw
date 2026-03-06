# AGENTS.md

## Commit Style
- Use Conventional Commits for all commit messages.
- Format: `<type>(<scope>): <description>`.
- Example: `fix(config): expand workspace path handling`.

## Database Evolution Policy
- Do not add migrations or backward-compatibility handling by default.
- Implement the current schema/behavior directly unless the user explicitly asks for migration/back-compat support.
