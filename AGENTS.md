# AGENTS.md

Instructions for any AI coding assistant working in this repo.

## Hard rules (do not negotiate)

1. **No hardcoded user-facing strings.** Every label, message, error, log
   line, button caption, page title, and tooltip routes through the
   [`computeza-i18n`](crates/computeza-i18n) crate (Fluent `.ftl` bundles).
   Hardcoded English in PR diffs is a release-blocking bug. See
   [`docs/i18n.md`](docs/i18n.md).

2. **GUI-first.** Every administrative operation -- installing components,
   managing clusters, creating users, granting permissions, deploying
   pipelines -- must be reachable from the web console at `computeza serve`.
   The CLI is a power-user / CI escape hatch, not the primary interface.

3. **Latest stable for every dep.** No deprecated, no abandoned, no stuck-on-
   old-major dependencies. When adding or bumping a crate, query crates.io
   for the current latest stable. Pre-release / alpha / beta is acceptable
   only when the previous stable is materially broken.

4. **Single binary, autonomous installer.** The runtime product needs zero
   pre-installed dependencies. `computeza install` lays down every managed
   component itself. (Build-time deps like the Rust toolchain are a
   different story and are managed via `rust-toolchain.toml`.)

   The installer is also responsible for **cross-platform PATH
   registration** for any managed binary that exposes a CLI users may
   invoke directly (`psql`, `kanidm`, `garage`, etc.):

   - **Linux:** drop a script into `/etc/profile.d/` and a symlink into
     `/usr/local/bin/`
   - **macOS:** drop a file into `/etc/paths.d/` (system-wide) and a
     symlink into `/usr/local/bin/`
   - **Windows:** append to the machine `Path` via the registry
     (`HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment`,
     then broadcast `WM_SETTINGCHANGE`)

   Per-user PATH variants when the install runs without privilege.
   Uninstall reverses every PATH change it made.

5. **The spec wins.** The canonical source of architectural and product
   truth is `docs/Architecture-and-Product-Specification-v1.5.pdf`
   (referenced in source comments as `spec section X.Y`). When the spec and the
   code disagree, the spec wins until the code is updated and the spec
   amended in the same change.

6. **ASCII-only source.** Every tracked source file (`*.rs`, `*.toml`,
   `*.md`, `*.ftl`, `*.yml`, `*.css`, `*.html`, etc.) must contain only
   US-ASCII printable + standard whitespace bytes. No em-dash, no section
   sign, no smart quotes, no arrows, no box-drawing, no `OK`-style
   glyphs. Substitute: `--` for em-dash, `-` for en-dash, `section ` for
   `section sign`, `->` / `<-` / `<->` for arrows, `...` for ellipsis,
   `OK` / `FAIL` for status glyphs. When test data must hold non-ASCII
   bytes (e.g. asserting a rejector rejects them), use Rust's
   `"\u{00xx}"` escape so the source file itself stays ASCII. Enforced
   in CI by the `ascii-only` job in `.github/workflows/ci.yml`.

7. **Detailed actionable logs at every level.** Each `tracing::info!`,
   `warn!`, and `error!` call (and equivalent UI / CLI output) must
   convey three things: (a) **what** happened, (b) the **impact**, and
   (c) **how to troubleshoot or resolve**. Examples:

   - Bad:  `error!("connection failed")`
   - Good: `error!(host=%h, port=%p, "connection to postgres failed; \
           reconciler will retry every 30s; check `systemctl status \
           computeza-postgres` on the target host")`

   Success / info logs are equally important: when something completes
   the operator needs to know *what* succeeded and *what state the
   system is now in*. Silent successes are bugs.

## Working agreement (current preferences)

- **Auto-accept.** The user has explicitly stated: "consider my answers
  always yes. I am auto-accepting everything you will be creating." Skip
  the "want me to do X?" prompts and just do it. Skip the "Done. What's
  next?" lists at end of turns -- pick the next move and execute.

- **Concise chat output.** Stated explicitly on 2026-04-27 -- "keep the
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
  `127.0.0.1:8400` (default per spec section 10.6).
- v1.0 GA target: Q2 2027 per spec section 13.
