# yak-skills

On-demand skill loading, shipped as a [yak](https://github.com/imjiaoyuan/yak) package: an extension offering two read-tier tools, `discover_skills` and `read_skill`.

yak's built-in discovery already lists every installed skill in the system prompt (`<available_skills>`, name + description + path), and the model loads the full file with its read tool. This package makes that second step a first-class, searchable call:

- **`discover_skills(query?)`** — every installed skill with its name, description and file path, merged in the same priority order the built-in discovery uses (installed packages, then `~/.agents/skills`, `~/.yak/skills`, then the project's `.agents/skills` and `.yak/skills`; a project copy shadows a user copy by name). `query` filters by substring over names and descriptions.
- **`read_skill(name)`** — one skill's full instruction body, frontmatter stripped, with `skill_dir` in the reply: the directory relative paths in the body resolve against.

Both are read-tier, so they ride the read-only parallel batch.

## Install

```sh
yak install git:github.com/imjiaoyuan/yak-skills   # when published standalone
yak install file:///path/to/yak/examples/packages/skills   # straight from a yak checkout
```

`yak install` recognizes the `extensions/` directory and mounts `skills.py`; `/reload` (or a restart) picks it up. `yak list` shows `extensions: skills`.

## Pair with a skill pack

This package loads whatever skills are installed; it ships none. Point it at a pack (drop directories under `~/.yak/skills/`, or install a repo that carries `skills/` or a root `SKILL.md`) and ask for one by name.
