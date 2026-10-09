---
name: commit-hygiene
description: Before committing in this repo, audit the staged change - one focused commit, imperative lowercase subject, only session files staged, fmt and clippy clean; use for pre-commit sweeps and history cleanups
---

# Commit hygiene

A commit is a review unit and a release note; both break when it sprawls.

## The rules

1. **One focused change.** If the subject needs "and", it is two commits.
2. **Subject line only**: imperative, lowercase, no prefix
   (`add interactive agent repl with slash commands`, never
   `FEAT: Added Interactive REPL!!`), no body bullets unless the *why* is
   genuinely invisible in the diff.
3. **Stage only what this session changed.** Never `git add -A` over a tree
   that carries unrelated tracked files; a file that is
   tracked-but-should-be-ignored goes in no commit, ever.
4. **The gates pass before the commit, not after**: `cargo fmt`, then
   `cargo clippy --all-targets` at zero warnings, then `cargo test`.

## The sweep

Run this before writing the commit:

1. `git status` — name every modified file and tie it to a step of the task;
   anything untied gets investigated or reverted, not committed.
2. `git diff --stat` — a change 10x the subject's claim means the subject
   is wrong or the commit is two.
3. `git diff --cached` after staging — the staged patch is the commit;
   the unstaged remainder is the next one.

## Fixing history

- A bad tip is `git commit --amend`; anything older that must change is a
  rebase, and a rebase over pushed shared history needs the user's explicit
  go-ahead first — ask, never force.

## See also

`references/subjects.md` holds worked examples of good and bad subjects
with the reasoning; read it when judging a borderline case.
