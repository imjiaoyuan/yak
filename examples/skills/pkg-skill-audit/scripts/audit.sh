#!/bin/sh
# The raw walk of one installed package: every file that yak's discovery
# would mount, one per line, prefixed by kind. Usage: audit.sh <pkg-dir>
set -eu
pkg="${1:?usage: audit.sh <pkg-dir>}"
[ -d "$pkg" ] || { echo "not a directory: $pkg" >&2; exit 1; }

# a whole-repo skill: SKILL.md at the package root
if [ -f "$pkg/SKILL.md" ]; then
    echo "skill (repo root) $pkg/SKILL.md"
fi

# skills/<name>/SKILL.md, the standard layout
if [ -d "$pkg/skills" ]; then
    for d in "$pkg"/skills/*/; do
        case "$d" in *"/") d="${d%\/}";; esac
        [ -f "$d/SKILL.md" ] && echo "skill $d/SKILL.md"
    done
    # flat skills/<name>.md files also mount
    for f in "$pkg"/skills/*.md; do
        [ -f "$f" ] && echo "skill (flat) $f"
    done
fi

# extensions mount as they stand: executables and manifest-header scripts
if [ -d "$pkg/extensions" ]; then
    for f in "$pkg"/extensions/*; do
        [ -f "$f" ] && echo "extension $f"
    done
fi

# commands/<file>: prompt templates
if [ -d "$pkg/commands" ]; then
    for f in "$pkg"/commands/*; do
        [ -f "$f" ] && echo "command $f"
    done
fi
