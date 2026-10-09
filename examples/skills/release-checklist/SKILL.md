---
name: release-checklist
description: Cut a release of this project end to end - verify the version bump, changelog or release notes, tags, build artifacts and the publish step, in that order, refusing to publish anything the checks below fail on
---

# Release checklist

Walk every gate in order. A release that skips one is a release rolled back.

## 1. The tree is releasable

- `cargo fmt` clean, `cargo clippy --all-targets` at zero warnings, `cargo test` green.
- The version in the manifest matches the tag about to be cut.
- If this project keeps a changelog or generates release notes from commits,
  every commit subject since the previous tag reads as a user-facing sentence.

## 2. The artifact builds

- Build with the release profile; a debug-profile artifact is not a release.
- Any installer, checksums or bundle the release pipeline ships must be
  regenerated against *this* build, not a stale copy.

## 3. The tag and the notes

- Tag `v<version>` on the commit that carries the version bump.
- Release notes list every subject since the previous `v*` tag; filter out
  version-bump commits themselves.

## 4. The publish

- Publish artifacts first, notes second: notes that link missing artifacts
  are a support incident.
- Verify the published artifact downloads and runs (`--version` is enough)
  before announcing anything.

## Refusing

Stop and report instead of publishing when:

- a check fails and the fix is not obvious within the task,
- the working tree is dirty in files the release touches,
- the tag already exists (a moved tag is a lie every cached client believes).
