# Releasing

Releases are cut from `main` with [cocogitto](https://github.com/cocogitto/cocogitto),
the Conventional Commits / SemVer toolbox. There is a single version for the
whole workspace; it lives once in `[workspace.package] version` in the root
`Cargo.toml` and every crate inherits it with `version.workspace = true`.

## Prerequisites

Install the tools the release process uses:

```bash
cargo install cocogitto      # provides `cog`
cargo install cargo-edit     # provides `cargo set-version`
cargo install cargo-nextest  # provides `cargo nextest`
```

## Commit conventions

History must follow [Conventional Commits](https://www.conventionalcommits.org/).
cocogitto derives the next version from the commits since the last tag:

| Commit | Bump |
|--------|------|
| `fix:` | patch (`0.1.0` → `0.1.1`) |
| `feat:` | minor (`0.1.0` → `0.2.0`) |
| `feat!:` / `BREAKING CHANGE:` | major (`0.1.0` → `1.0.0`) |

`chore:` and `test:` commits are kept out of the changelog. Merge commits are
ignored.

## Local setup

Install the shareable commit-msg hook once per clone. Hooks under `.git/hooks`
are not versioned, so this is a local step:

```bash
cog install-hook --all
```

The hook runs `cog verify` on the message and `cog check` on the history, then
the same formatting and lint gates as CI. CI also enforces `cog check` on every
push and pull request.

Validate the configuration at any time:

```bash
cog check        # conventional-commit check over the history
cog changelog    # preview the unreleased changelog
```

## Cut a release

1. Open the repository on GitHub.
2. Go to **Actions → Release → Run workflow**.
3. Pick the branch (`main`) and a bump type — `auto` (default), `major`,
   `minor`, or `patch` — then run it.

The workflow then:

1. Checks out the full history (`fetch-depth: 0`) and installs Rust,
   `cargo-edit`, and `cargo-nextest`.
2. Runs `cog bump`, which executes the pre-bump hooks: `cargo set-version
   --workspace <version>`, then the CI gates (`cargo fmt`, `cargo clippy
   --locked`, `cargo nextest --locked`, the docs reference regeneration).
3. Commits the bumped `Cargo.toml`, `Cargo.lock`, and `CHANGELOG.md` as the
   version commit, creates the `vX.Y.Z` tag, and pushes both.
4. Generates release notes from the changelog and creates the GitHub release.

`cargo set-version` updates `[workspace.package] version` and `Cargo.lock`
together, so `--locked` builds stay reproducible.

## Version source

Never edit the version by hand. It lives in `[workspace.package] version` in the
root `Cargo.toml` and is written by the pre-bump hook during a release.

With no existing tags, the first `cog bump --auto` produces `v0.1.0`, matching
the current manifest. To start at a different version, cut the first release
with `cog bump --version X.Y.Z` instead.

## Not covered

Publishing to crates.io is out of scope: `favetto-core` and
`favetto-providers` are internal path dependencies. Adding crates.io publishing
is a separate decision.
