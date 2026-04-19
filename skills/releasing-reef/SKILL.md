---
name: releasing-reef
description: How to cut a reef release — version bump, tag push, and the GitHub Release notes format. Use when the user says "cut a release", "tag vX.Y.Z", "打个 tag 发布", "发布", "bump version", "release notes", or asks "how do I release reef", "what's the notes format", "is it safe to push this tag". Covers the full pipeline pushing a `v*` tag triggers (multi-platform binary build + npm publish to `@reef-tui/cli` + GitHub Release with assets), the notes convention, and the specific traps we've paid for — Cargo.toml drift, shell escaping when passing notes inline, and how `softprops/action-gh-release` behaves with pre-existing release bodies.
---

# Releasing reef

Reef releases are driven by **git tags matching `v*`**. Pushing such a tag is the single trigger for `.github/workflows/release.yml`, which does the rest automatically: binaries for five targets, npm publish of `@reef-tui/cli` + five platform subpackages, GitHub Release with assets. You never run `cargo publish`, never `npm publish` by hand, never upload binaries manually. The contract is **bump Cargo.toml, commit, tag, push**.

## What a `v*` tag push actually runs

`.github/workflows/release.yml`:

1. **Build matrix** — `cargo build --release --locked` for:
   - `aarch64-apple-darwin`, `x86_64-apple-darwin`
   - `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (via `cross`)
   - `x86_64-pc-windows-msvc`
2. **npm platform subpackages** — publishes `@reef-tui/cli-{darwin-arm64,darwin-x64,linux-arm64,linux-x64,win32-x64}`. Each subpackage's `package.json` version is overwritten at publish time to `${TAG#v}`.
3. **npm main package** — publishes `@reef-tui/cli` with `optionalDependencies` pinned to the same version.
4. **GitHub Release** — via `softprops/action-gh-release@v2`:
   - Attaches `reef-{darwin,linux}-*.tar.gz` and `reef-win32-x64.zip` archives.
   - Has `generate_release_notes: true` — this **appends** auto-generated `## What's Changed` notes to whatever body the release already has; it does not overwrite a pre-existing body.

Implications:
- The source `package.json` versions (`0.0.0`) are placeholders — the tag is authoritative for npm.
- Deleting a tag after push does **not** unpublish npm packages. You have a 72h unpublish window on npm; beyond that, cut a patch release.
- `NPM_TOKEN` repo secret must be valid. Expired token → silent failure on npm steps while the GitHub Release still succeeds.

## Prerequisites

Before touching versions:

```bash
git checkout main
git fetch origin
git pull --ff-only
git status                                         # must be clean
gh pr list                                         # nothing should-ship-in-this-release
gh run list --workflow=ci.yml -b main --limit 3    # all green
```

If CI is red on main, **fix it before tagging**. A tag on a red commit is a broken release.

## Choosing the version

Repo uses pre-1.0 SemVer, loosely:

| Change | Bump |
|---|---|
| Bug fixes only | `0.x.y+1` (patch) |
| Any new user-visible feature | `0.x+1.0` (minor) |
| Breaking change (pre-1.0) | `0.x+1.0` — minor still; breaking is expected below 1.0 |
| Stability commitment | `1.0.0` — deliberate, not as part of a routine release |

Skim the commit list to classify:

```bash
PREV=v0.4.0
git log --oneline --no-merges "${PREV}..main"
```

If every commit is `fix(...)` → patch. One or more `feat(...)` → minor. When in doubt, ask the user — don't guess.

## Procedure

```bash
PREV=v0.4.0       # last tag — sanity-check with `git describe --tags --abbrev=0`
NEW=v0.5.0        # this release

# 1. Bump Cargo.toml. `cargo build` re-runs to sync Cargo.lock's reef entry.
sed -i '' 's/^version = ".*"/version = "'"${NEW#v}"'"/' Cargo.toml   # macOS BSD sed
cargo build
git diff --stat Cargo.toml Cargo.lock              # both files, trivial diff

# 2. Release commit — on main, pre-tag.
git add Cargo.toml Cargo.lock
git commit -m "chore: release ${NEW}"
git push origin main

# 3. Annotated tag on the release commit, push it.
git tag -a "${NEW}" -m "${NEW}"
git push origin "${NEW}"

# 4. Write notes to a file, then pre-create the Release before the workflow finishes.
$EDITOR /tmp/release-notes.md
gh release create "${NEW}" --title "${NEW}" --notes-file /tmp/release-notes.md
```

Step 4 races the workflow's release-creation step (the `github-release` job runs after the ~3-minute build matrix). Pre-creating with `gh release create` wins the race and the workflow's action finds the existing release, attaches the binaries, and appends `## What's Changed` to your body. Your handwritten Highlights stay at the top.

If you lose the race (release already created by the workflow with only auto-notes), fix it up:

```bash
gh release edit "${NEW}" --notes-file /tmp/release-notes.md
```

Note: `--notes-file`, not `--notes "..."`. Inline heredoc gets mangled by shell escaping — see Pitfalls below.

## Release notes format

Template (`/tmp/release-notes.md`):

```markdown
## Highlights

- **Feature name** — one-sentence user-facing pitch. (#PR)

## Fixes

- Short description of the fix. Closes #ISSUE. (#PR)

**Full changelog**: https://github.com/Blushyes/reef/compare/vPREV...vNEW
```

Rules:

- **Headings**: `## Highlights` (new user-visible features) → `## Fixes`. Add `## Breaking changes` at the top if any.
- **One bullet per user-visible change**, not one per commit. Fold related commits into a single bullet.
- **Bold the feature name** at the start of a Highlights bullet; plain prose in Fixes.
- **`Closes #N` inline** for user-reported issues the release resolves.
- **`(#PR)` at the end** of every bullet — helps readers jump into the discussion.
- **Always end with the compare link** — it's the exhaustive view.
- **No emojis**.
- **Omit `chore:`, `refactor:`, `test:`, `ci:`, `docs:`** unless they change user-visible behavior. The tag's auto-appended `## What's Changed` will list all PRs regardless.

Do *not* manually write a `## What's Changed` or list every PR — the workflow appends that from GitHub's auto-generated notes. Don't duplicate.

Example — the actual v0.5.0 notes we shipped:

```markdown
## Highlights

- **Vim-style in-panel search** — `/` / `?` to search, `n` / `N` to jump between matches, works across all three tabs. (#11)
- **Open files in `$EDITOR`** — press <kbd>Enter</kbd> on a file in the Files tab, or `e` on a changed file in the Git tab, to hand off to your configured editor. (#12)

## Fixes

- Scrolling is no longer trapped on the opened file in the **Files** tab — the mouse wheel now scrolls the tree freely again. Closes #10. (#13)
- Same fix applied to the **Graph** tab so the commit list scroll isn't snapped back to the selected commit. (#14)

**Full changelog**: https://github.com/Blushyes/reef/compare/v0.4.0...v0.5.0
```

## Post-release verification

```bash
gh run watch --exit-status                         # wait for release.yml to finish
gh release view "${NEW}"                           # binaries all listed?
npm view @reef-tui/cli version                     # matches ${NEW#v}?
npm view @reef-tui/cli-darwin-arm64 version        # and subpackages
```

Expected assets on the Release:
- `reef-darwin-arm64.tar.gz`, `reef-darwin-x64.tar.gz`
- `reef-linux-arm64.tar.gz`, `reef-linux-x64.tar.gz`
- `reef-win32-x64.zip`

If any are missing, the matrix job for that target failed — open `gh run view <run_id>`, check which job, re-run just the failed jobs with `gh run rerun <run_id> --failed`. A partial build does **not** block the npm publish jobs that already ran successfully, so your npm package may be live without the missing platform. Decide: rerun (good if the failure was transient) or cut a patch release (if the failure is real).

## Pitfalls we've paid for

### Cargo.toml drift

Before v0.5.0, Cargo.toml sat at `version = "0.1.0"` across seven tags from v0.1.0 → v0.4.0. Every release commit **must** bump Cargo.toml + Cargo.lock together. If you see `version = "0.x.y"` where x.y is older than the latest tag on main, the previous release skipped the bump — fix it in your release commit.

Consider adding a CI check later: on tag push, assert `Cargo.toml` version equals `${GITHUB_REF_NAME#v}` before the publish jobs.

### Shell escaping in `--notes`

```bash
# DON'T — backticks and $ get interpreted by the outer "..." even with
# a single-quoted heredoc inside. Notes ship with literal \` and \$.
gh release create vX.Y.Z --notes "$(cat <<'EOF'
...
EOF
)"

# DO — write to a file first.
gh release create vX.Y.Z --notes-file /tmp/release-notes.md
```

v0.5.0 shipped with `\`\$EDITOR\`` visible in the rendered body — not a disaster but ugly. Use `--notes-file` every time there's a code fence, backtick, or `$` in the notes.

### Racing the workflow

If you push the tag and walk away, the workflow creates the Release itself (~3 min) with only GitHub's auto-generated notes. Write and pre-create your notes immediately after `git push origin vX.Y.Z`, or fix up afterwards with `gh release edit --notes-file`.

### macOS `dtolnay/rust-toolchain@stable` DNS flakes

The `Install Rust toolchain` step on `macos-14` sometimes fails with `failed to lookup address information` resolving `static.rust-lang.org`. Transient. Rerun just the failed job:

```bash
gh run rerun <run_id> --failed
```

Does not require retagging or rebuilding.

### Don't delete a pushed tag to "undo"

Deleting the tag from GitHub does not unpublish npm packages. If you catch a bad tag within 72h, `npm unpublish @reef-tui/cli@$VER` and each subpackage (5 of them). Beyond 72h, cut a patch release that reverts the offending commit — npm has no public unpublish after that window.

### Prereleases

The current pipeline does not branch on prerelease tags. Pushing `v0.6.0-rc.1` would publish it to npm under the default `latest` dist-tag, which is wrong for a prerelease. If prereleases become needed, guard the publish jobs on `!contains(github.ref, '-')` or add a `--tag next` flag to `npm publish`. For now: don't prerelease through this pipeline.
