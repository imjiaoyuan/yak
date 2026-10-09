---
name: pkg-skill-audit
description: Audit an installed yak package for what it carries and mounts - walk its skills/, extensions/ and commands/ trees, cross-check against pkg::carved classification and the mounted surfaces, and report anything that mounts but was not declared
---

# Package skill audit

An installed package (`~/.yak/pkg/<name>` or `.yak/pkg/<name>`) mounts
everything it carries. This skill audits the gap between what a package
*declares* (README, install report) and what it *mounts*.

## The walk

1. List the package root. Every one of these mounts:
   - `skills/<name>/SKILL.md` — one skill each, plus a root `SKILL.md`
     counting as a whole-repo skill.
   - `extensions/<file>` — resident extensions and manifest script tools.
   - `commands/<file>` — prompt templates.
2. For each discovered item, decide whether the package's own docs
   (README, install output) name it. Unnamed-but-mounting is the finding.
3. Check for shadowing: a package skill or extension named like one already
   in `~/.yak/skills` / `~/.yak/extensions` overrides by the discovery
   order (project beats user, our dirs beat interop dirs). A package that
   silently overrides a user tool is a finding, not a feature.

## The report

Report, in this order:

- what mounts (name, kind, source path),
- what was declared but does not mount (broken layout — e.g. a SKILL.md
  without a description, an extension that fails its handshake),
- what mounts but was never declared,
- override conflicts with user-installed items.

## Running the checks

`scripts/audit.sh <pkg-dir>` prints the raw walk; read its output, do not
trust it blindly — the judgment calls (declared? shadowing?) are the skill's,
not the script's.
