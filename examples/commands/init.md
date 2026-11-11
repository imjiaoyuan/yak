Explore this repository as it is now, then create or refresh `AGENTS.md` at its root: the single guide a future agent reads before changing anything. Do not start writing until you have explored.

Explore first, but keep it bounded: read `README.md`, the existing `AGENTS.md` (or `AGENTS.override.md` / `CLAUDE.md`), and the docs index; list the repository root and the top-level source directories; read the build manifest (`Cargo.toml`, `package.json`, `pyproject.toml`, `Makefile`, ...), the CI config and any release script to learn the exact commands; sample source files only where the docs are silent about architecture or conventions. Prefer recently changed areas over exhaustive reading: the goal is the current tree, not a full audit.

Then write `AGENTS.md`, keeping only the sections that earn their place:
- **What this is** — one short paragraph: the project's purpose and shape.
- **Commands** — build, test (including how to run a single test), lint/format, and run; copy them from the manifest and CI, never invent flags or paths.
- **Architecture** — a compact map, one line per module or directory, and a pointer to where the deeper docs live.
- **Conventions** — the rules a change must follow and the constraints it must not break (state, errors, naming, commits).
- **Storage / data layout** — only if the project has one.

Rules for the file: state each fact once, in its home; never restate what `README.md` or `docs/` already owns — link to it instead; favor the map and the constraint list over prose; verify every command against the repository before writing it; write in the language and markdown style the existing docs use; do not add generic advice (`write tests`, `keep code clean`), exhaustive file trees, generated code, or anything one `ls` already answers.

If `AGENTS.md` already exists, update it to match the current tree instead of rewriting from scratch: read it in full first, keep every section that is still correct, correct stale facts, add what is missing, and never delete unique content just to make it shorter. If it is already accurate and complete, leave it unchanged and say so. Never touch `AGENTS.override.md`; if `CLAUDE.md` exists beside the target, use it as source material and leave it untouched.

Write the file yourself with the `write` or `edit` tool; do not paste its contents into your reply. Finish with a short summary of what changed.

Additional focus from the user, if any, follows.
