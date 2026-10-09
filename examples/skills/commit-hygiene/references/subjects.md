# Subject lines, worked

Good subjects name the user-visible change in imperative lowercase:

- `add interactive agent repl with slash commands`
- `move the ls tool onto the extension surface as examples/extensions/ls`
- `replace the webfetch built-in with a stdlib-urllib example extension, byte-compatible`

Bad ones, and why:

- `fix` — names nothing; a history search for the fix finds every commit.
- `Fix Bug in EditTool` — capitalized and names the component file, not the
  behavior; the release notes would read "Fix Bug in EditTool".
- `update code + docs + tests` — three changes, one commit, and "update"
  is what git already knows from the diff.
- `refactor: extract truncation helpers` — the prefix is noise in a
  single-contributor repo and breaks the lowercase rule; the colon format
  belongs to convention-driven teams.
- `it works now` — describes the author's relief, not the change.

The borderline case: a change touching code and its docs in the same motion
(one behavior, both halves) is *one* commit. A change touching two behaviors
(a fix plus an unrelated cleanup it noticed) is two, even when small.
