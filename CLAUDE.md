# CLAUDE.md

Instructions for Claude (and any other AI assistant) working in this repo.

## Hard rules (do not negotiate)

1. **No hardcoded user-facing strings.** Every label, message, error, log
   line, button caption, page title, and tooltip routes through the
   [`computeza-i18n`](crates/computeza-i18n) crate (Fluent `.ftl` bundles).
   Hardcoded English in PR diffs is a release-blocking bug. See
   [`docs/i18n.md`](docs/i18n.md).

2. **GUI-first.** Every administrative operation — installing components,
   managing clusters, creating users, granting permissions, deploying
   pipelines — must be reachable from the web console at `computeza serve`.
   The CLI is a power-user / CI escape hatch, not the primary interface.

3. **Latest stable for every dep.** No deprecated, no abandoned, no stuck-on-
   old-major dependencies. When adding or bumping a crate, query crates.io
   for the current latest stable. Pre-release / alpha / beta is acceptable
   only when the previous stable is materially broken.

4. **Single binary, autonomous installer.** The runtime product needs zero
   pre-installed dependencies. `computeza install` lays down every managed
   component itself. (Build-time deps like the Rust toolchain are a
   different story and are managed via `rust-toolchain.toml`.)

5. **The spec wins.** The canonical source of architectural and product
   truth is `docs/Architecture-and-Product-Specification-v1.5.pdf`
   (referenced in source comments as `spec §X.Y`). When the spec and the
   code disagree, the spec wins until the code is updated and the spec
   amended in the same change.

## Working agreement (current preferences)

- **Auto-accept.** The user has explicitly stated: "consider my answers
  always yes. I am auto-accepting everything you will be creating." Skip
  the "want me to do X?" prompts and just do it. Skip the "Done. What's
  next?" lists at end of turns — pick the next move and execute.

- **Concise chat output.** Stated explicitly on 2026-04-27 — "keep the
  output very short to limit token usage". The durable record lives in
  TodoWrite, git commits, and code-doc comments; the chat is just a thin
  "here's what's happening now" layer.

- **Always run the full local CI gate before committing:**
  ```sh
  cargo fmt --all
  cargo check --workspace --all-targets
  cargo clippy --workspace --all-targets
  cargo test --workspace
  ```
  CI is `--locked`, so `Cargo.lock` must always be committed when deps change.

- **Commit style.** Subject line uses the affected crate as prefix
  (`reconciler-postgres: ...`, `ui-server: ...`, `ci: ...`). Body explains
  *why*, not just *what*. Co-author trailer on every commit.

## Workspace conventions

- Crate prefix is `computeza-*` (the spec's `platform-*` is a placeholder).
- The `i18n` crate is `computeza-i18n` (not `ui-i18n`); strings for any
  surface go in `crates/computeza-i18n/locales/<lang>/<bundle>.ftl`.
- Rust toolchain is pinned to `channel = "stable"` in
  [`rust-toolchain.toml`](rust-toolchain.toml); MSRV declared in
  [`Cargo.toml`](Cargo.toml).
- Workspace deps centralised in `[workspace.dependencies]`; every crate
  uses `{ workspace = true }` references.

## Project state notes

- Pre-alpha. Most reconciler crates are stubs. `computeza-core`,
  `computeza-i18n`, `computeza-reconciler-postgres`, `computeza-ui-server`,
  and the `computeza` binary have real implementations.
- `cargo run --bin computeza -- serve` boots the operator console at
  `127.0.0.1:8400` (default per spec §10.6).
- v1.0 GA target: Q2 2027 per spec §13.
