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

## Product constraints

These are durable business / go-to-market constraints that shape
architecture across the codebase. Engineering decisions touching the
listed surfaces must preserve them.

### Multi-tier distribution channels

Computeza is sellable through three channels simultaneously, and the
codebase must not bake in any single one of them:

1. **Direct** -- Computeza -> end-customer.
2. **Reseller** -- Computeza -> reseller -> end-customer.
3. **Sub-reseller** -- Computeza -> reseller -> sub-reseller ->
   end-customer (modelled on the Databricks -> Microsoft -> enterprise
   pattern).

Surfaces this affects and how engineering should treat them:

- **Licensing tokens** must encode the full reseller chain
  (`issuer -> tier 1 -> tier 2 -> customer`), and activation must round-
  trip the chain back so every upstream party can verify entitlement
  and bill. The current ed25519-signed license model needs a "chain"
  claim before stable.
- **White-labeling.** Any tier in the chain may rebrand the operator
  console (logo, brand mark, accent colors, support contact). Keep
  `assets/computeza.css` CSS-variable-driven; the `cz-brand` element
  needs to accept a tenant-supplied SVG once branding lands.
- **Telemetry / metering** must flow upward for reseller billing
  without exposing customer-private content to upstream tiers. Plan
  a coarse "seats / components installed / query volume" pipe where
  each tier sees only aggregates of its downstream.
- **Multi-tenancy.** `ResourceKey::workspace_scoped` already exists;
  v0.1 should grow workspaces into first-class objects so one
  Computeza deployment can serve "one reseller's tenant" or "one
  end-customer" cleanly.
- **Channel-partner API.** Resellers will need a provisioning API
  (likely gRPC + mTLS) separate from the operator console.
- **Support routing.** Error reporting must route to the operator's
  vendor-of-record, which may not be Computeza itself. Plan a
  `support_contact` config knob shown in the footer and linked from
  error pages.

v0.0.x ships single-tenant direct-use. The constraints above belong
in the design when these surfaces are first built; do not paint into
a single-tier corner.

## Component installer playbook (postgres lessons applied)

The PostgreSQL native installer was the first end-to-end install path
to land. It hit nearly every pitfall an autonomous installer can hit,
and the fixes are now infrastructure other components MUST reuse
rather than rediscover. When adding a new component reconciler +
driver pair, check this playbook before pretending any step is simple.

### Verify the distribution channel BEFORE writing a driver

The autonomous-binary-download pattern in `fetch::Bundle` only works
for components whose vendor publishes a binary tarball / zip / raw
binary at a stable URL. Before assuming that pattern fits a new
component, **verify the vendor's actual distribution channel** via
the GitHub Releases API or equivalent. The kanidm pass got this
wrong: every recent v1.x release tag had **0 binary assets attached**
(distribution is via distro package managers + Docker + `cargo
install`), so the download-from-GitHub Bundle URLs were 404s from
the start.

Quick API check before pinning URLs:

```sh
curl -s "https://api.github.com/repos/<vendor>/<repo>/releases/tags/<tag>" \
  | python -c "import sys, json; d=json.load(sys.stdin); print(len(d.get('assets',[])))"
```

If the asset count is 0, the component does not fit the
`fetch::Bundle` pattern. Pick one of:

- **Package-manager dispatch** (apt / dnf / zypper / brew / pacman /
  apk / pkg). Detect the host PM, shell to its install command,
  trust it to place binaries on PATH. Works for kanidm, garage,
  greptime when distro packages exist.
- **`cargo install`** with a pinned `--version`. Requires the Rust
  toolchain on the host; slow but always works for pure-Rust
  components published on crates.io.
- **Bundled container runtime**. Conflicts with the no-Docker spec
  mandate; do not pursue.
- **Vendor's own CDN**. Some components (Garage, Grafana, Databend)
  publish binaries on their own infrastructure rather than GitHub
  releases. The Bundle URL points there directly.

The kanidm driver retains its 3-OS structure + wizard + uninstall
even with the broken URLs; the package-manager dispatch slots into
`fetch_and_extract`'s position when it lands. The card stays
`available: false` until then to keep the hub honest.

### Binary acquisition

- **Three-tier resolution.** The driver must try, in order:
  1. Caller-supplied `bin_dir` (operator override).
  2. Host-installed location (`/Program Files/<Vendor>/<v>/bin` on
     Windows, `apt`/`brew` paths on Linux/macOS).
  3. Computeza-managed cache under `<root_dir>/binaries/<version>/`.
  4. Download from vendor + extract into the cache.
- **Stream the download to disk** (not `bytes().await`), report
  `bytes_downloaded` / `total_bytes` through `ProgressHandle` so the
  wizard's bar moves.
- **Pin a SHA-256 per bundle.** TLS protects in-transit; the pin
  protects against a vendor-side incident. Audit-trail entries go in
  AGENTS.md when checksums change.
- **`.computeza-extracted` sentinel** marks a fully-extracted cache
  dir so re-runs hit the cache without re-downloading.

### Service registration

- **Never `sc.exe create` against a bare binary on Windows.**
  Component binaries (`postgres.exe`, etc.) are console apps that
  don't speak the SCM control protocol; SCM gives up after 30s with
  error 1053. Use the component's own service-registration tool
  (`pg_ctl register`) or write a service-aware wrapper.
- **SCM has DELETE_PENDING.** After `sc delete` the service stays in
  the SCM until every handle closes (Services snap-in, sc.exe query
  loops). Always run `wait_for_service_absent` before re-registering;
  the pg_ctl equivalent will fail with `service "..." already
  registered` otherwise.
- **Layer the teardown.** Tear a service down through every path
  available: `sc::stop`, the component's own unregister, `sc::delete`,
  then poll until SCM evicts.

### Config files

- **Many config files are first-match-wins.** `pg_hba.conf`,
  `nginx.conf` `location` blocks, etc. Computeza-managed rules MUST
  be **prepended** with sentinel comments (`# === computeza-managed
  ... (start) ===` / `(end) ===`), never appended.
- **Idempotent rewrites.** Strip the previous managed block by
  sentinel before writing the new one. Also strip *unmarked* legacy
  lines from earlier driver versions so re-installs converge to a
  clean file.
- **Pure-string transforms unit-test.** Factor `rewrite_pg_hba(s) ->
  String` out of `ensure_loopback_trust(data_dir)` so the rewrite
  logic is testable without touching the disk.

### OS-user gotchas

- **Always pass `-U <expected-superuser>`** (or equivalent) to
  bootstrap commands. Tools default to `%USERNAME%` on Windows /
  `$USER` on Unix, which on an admin-elevated session is the wrong
  identity. `initdb -U postgres` is the canonical example.
- **For existing installs without the explicit user**, ship an
  `ensure_<role>` post-install step that runs `IF NOT EXISTS ...
  CREATE ROLE ...` against the running service via the OS user that
  bootstrapped it. Idempotent. Bridges old data dirs without
  forcing the operator to re-initialise.

### Auth bootstrap

- **Trust on loopback** (`127.0.0.1/32 trust` + `::1/128 trust`) is
  the v0.0.x default. It enables the in-process reconciler to
  observe without a secret store. Loopback-only binding makes this
  safe; remote connections still require scram-sha-256.
- **Defer password input** until `computeza-secrets` lands. The
  install wizard's `<details>` advanced section can grow a password
  field that writes through the secret store rather than into the
  spec JSON directly.

### Cross-platform PATH

- **Windows PowerShell does NOT use `\` for line continuation.** Use
  backtick (`` ` ``), but really: write single-line `-Command`
  invocations with `;` separators. Multi-line scripts via `-Command`
  are fragile.
- **Use `Split(';') -contains` for PATH membership tests**, not
  `-notlike "*<dir>*"` -- the wildcard match false-positives on
  paths containing the candidate as a substring.
- **Always test the actual permission**, not just "command succeeded
  silently". Surface the registration error in `Installed.<thing>_error`
  so the result page shows the verbatim diagnostic instead of
  "(not created)".

### Reconciler integration

- **Spec + Status separation.** The install wizard writes only the
  spec (endpoint config) to the store. The reconciler is responsible
  for the Status (observed state). Don't muddle the two.
- **Upsert on re-install.** `store.save(.., None)` is *create-only*
  -- a second install hits "revision conflict". Load first to
  discover the current revision, then save with `expected_revision =
  Some(rev)`.
- **Kick an immediate observe after install completes.** The 30s
  periodic tick is too coarse for "did my install actually wire up?"
  feedback. The handler calls the reconciler's `observe()` once
  before redirecting to the result page so /status is current by
  the time the operator clicks through.
- **Loopback trust auth + empty password in the spec.** Until the
  secret store ships, `superuser_password: SecretString::from("")`
  is fine -- pg_hba.conf's trust rule wins on 127.0.0.1.

### Wizard UX

- **POST-redirect-GET with a job ID.** Install runs in a tokio task;
  the POST returns 303 to `/install/job/{id}`. The job page polls
  `/api/install/job/{id}` every 500ms for live progress. Avoids the
  browser-timeout problem on large downloads.
- **`<details>` for advanced options.** Default-everything stays
  one click; per-component overrides (port, data dir, service
  name) tuck into a disclosure.
- **Uninstall is a first-class flow.** Every component install MUST
  ship a counterpart teardown with a confirmation page, a danger-
  styled button, and a step-by-step result page. The teardown
  preserves the binary cache by default (re-install stays fast)
  but exposes a `purge_binaries: true` opt-in for full rollback.

### Parallel-install safety

Operators routinely want two versions of the same component side by
side (a v17 production cluster + a v18 staging cluster). The
collisions to design around:

- **Service name collision.** SCM / systemd / launchd indexes by
  name. Default `computeza-<component>` collides on a second
  install. The wizard MUST suggest a version-suffixed name
  (`computeza-postgres-17`, `computeza-postgres-18`) when a
  Computeza-managed service of the same component already exists.
- **Port collision.** Default ports (5432 for postgres, 8443 for
  kanidm, etc.) won't be available for the second install. Suggest
  the next free port at or above the canonical default.
- **Data dir collision.** Sharing one data dir between two installs
  would corrupt both. Suggest a version-suffixed leaf
  (`postgres-17/`, `postgres-18/`) when colliding.
- **Binary cache.** Already version-keyed -- two installs share a
  cache, no collision.
- **PATH shim.** `computeza-<tool>` shim is last-writer-wins. Acceptable
  because the operator using the CLI directly tends to want the
  newest version; the install wizard can warn but doesn't need to
  prevent.

The `driver-native::detect` module is the shared infrastructure:

- `detect_installed()` per OS reports `DetectedInstall` records.
- `smart_defaults(detected, requested_version_major)` produces a
  port + service-name / data-dir suffix that won't collide.
- The wizard renders detected installs as a card above the form and
  feeds the defaults into the form's `placeholder` attributes.

New components must implement `detect_installed()` next to their
install path. The detection logic should be conservative: false
positives push operators into unnecessary suffixed names; false
negatives at worst surface as a "service already exists" error
that the wizard's existing teardown handles.

### Reseller / sub-reseller infrastructure

Per the durable product constraint in the "Product constraints"
section, every install/instance management surface needs to flow
through the multi-tier resale chain. For each new component:

- **Identifier shape.** `<component>-instance/<name>` resource keys
  must be workspace-scoped, not cluster-scoped, once multi-tenancy
  ships. Don't hardcode `cluster_scoped(...)` in places where
  `workspace_scoped(...)` will need to slot in.
- **Branding.** Render labels (component name, badges) through
  i18n, so resellers white-labelling the console can override
  per-tenant.
- **Metering tap.** When a component starts handling traffic, the
  reconciler's `observe()` is the natural place to record an
  aggregate (counts of users, buckets, collections, etc.). Don't
  let component-specific telemetry leak past the workspace
  boundary -- aggregate before exposing upward.
- **Provisioning API.** The CLI / web wizard are operator-facing;
  reseller-facing provisioning needs the gRPC `channel-partner`
  surface from the Product constraints section. Don't paint
  component installs into "operator clicks through wizard" as the
  only path -- expose the underlying `install(opts)` function so
  the API surface can drive it later.

### Per-component checklist

When adding a new component (kanidm, garage, lakekeeper, ...):

- [ ] `crates/computeza-driver-native/src/<os>/<component>.rs`
      with `install_with_progress(opts, &ProgressHandle)` and
      `uninstall(opts) -> Uninstalled` -- mirror the postgres shape.
- [ ] At least two pinned `Bundle`s (latest + previous-major) per
      OS. UI exposes a version dropdown.
- [ ] `ensure_loopback_trust`-equivalent for whatever auth this
      component uses (admin token bootstrap, trust on loopback, ...).
- [ ] `ensure_<role>` post-install step for cross-platform user-name
      drift if relevant.
- [ ] Reconciler reads the spec, calls `with_state`, runs `observe()`.
      Already shipped for all 10 HTTP reconcilers in `28057eb`.
- [ ] Wizard form on `/install/<component>` collects port + data dir
      + service name; defaults are blank fields with placeholders.
- [ ] Uninstall route + confirmation page wired into the wizard
      footer card.
- [ ] Smoke test that the install handler writes the
      `<component>-instance/local` row in the metadata store with
      the correct shape.
- [ ] End-to-end test: open in-memory SqliteStore, persist a fake
      observation, hit /status, assert the row renders.

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
- **v0.0.x is Linux-only for the data-plane install path -- every
  component, including postgres.** The 11 managed components target
  systemd-based Linux on x86_64. macOS + Windows native install drivers
  ship in v0.1+. The wizard explicitly refuses installs on non-Linux
  hosts via `guard_supported_os` (which calls into
  `computeza-driver-native::os_detect`). The macOS + Windows postgres
  driver modules at `crates/computeza-driver-native/src/{macos,windows}/postgres.rs`
  are reference code from earlier iterations and are no longer reachable
  through the wizard -- do NOT extend them for new components. The
  operator console (`computeza serve`) itself remains cross-platform;
  only install actions are Linux-gated.
- v1.0 GA target: Q2 2027 per spec section 13.

## Unified install (`/install`)

The operator console's `/install` page is the unified whole-stack
install form. One card per component lays out service-config inputs
(service name, port, data directory, version pin); a single Install
button at the bottom POSTs back to `/install`, which spawns one job
that runs every available component sequentially through
[`INSTALL_ORDER`] in `crates/computeza-ui-server/src/lib.rs`.

### Per-install persistence + lifecycle

Each component install in the unified flow persists **three** metadata
rows after a successful install:

1. `<slug>-instance/local` -- the spec the reconciler observes against
   (endpoint, databases, etc.). Already required by the reconciler.
2. `install-config/<slug>-local` -- the `InstallConfig` the operator
   chose (version pin, port override, data dir override, service name
   override). Persisted so rollback / repair can target the same
   service the install created instead of falling back to driver
   defaults.
3. `<slug>/admin-password` in the encrypted `SecretsStore`
   (only for components with an admin-credential concept: postgres,
   kanidm, grafana). Generated **after** a successful install so a
   failed install never leaves an orphan secret.

The rollback flow (`POST /install/job/{id}/rollback`) reads
`install-config/<slug>-local` for each Done component, builds an
`UninstallOptions` with the persisted `service_name` + `root_dir`
overrides, and tears down via `dispatch_uninstall_with_config`. After
each component uninstall it also drops the install-config row and the
`<slug>/admin-password` entry, so a torn-down component leaves no
metadata or credential residue.

The repair flow on `/resource/{kind}/{name}` reads the persisted
config and embeds it as hidden inputs on the Re-install form so the
re-run targets the same service the install created.

**Known gap (per-component pages):** the legacy per-component pages
at `/install/<slug>` and `/install/<slug>/uninstall` do NOT yet save
or read `install-config/<slug>-local`. Operators who mix flows
(install via `/install`, then teardown via `/install/postgres/uninstall`)
will hit the same custom-service-name miss the rollback flow used to
have. The fix is symmetric: each per-component install handler should
save install-config + generate credentials, and each per-component
uninstall handler should load install-config + dispatch through
`dispatch_uninstall_with_config`. Tracked as a v0.0.x follow-up.

Adding a new component to the unified install:

1. Append the slug to `INSTALL_ORDER` in the position that respects
   its dependencies (postgres-first, storage before query, etc.).
2. Add a match arm in `dispatch_install` that calls the component's
   `run_<slug>_install_with_progress` and returns the metadata-store
   spec shape for `<slug>-instance/local`.
3. Set `available: true` on the component's `ComponentEntry` once
   driver + reconciler are wired -- the unit test
   `install_order_only_lists_available_components` enforces that
   every `INSTALL_ORDER` entry is marked available.
4. Add the canonical default port to `canonical_defaults_for` so the
   form placeholder shows the right number.

The per-component pages (`/install/postgres`, `/install/kanidm`, ...)
remain at their existing routes so power users / CI scripts can drive
one install at a time; they are no longer linked from the hub.

The unified flow's per-card "Identity and access" disclosure is a
**v0.1+ placeholder** today -- service account, initial admin
credentials, group permissions, and upstream IdP federation (Entra
ID / AWS IAM / GCP IAM / on-prem LDAP / Kerberos) all configure
through it once the `computeza-secrets` install-time binding and the
identity-federation crate land. v0.0.x installs against loopback
trust auth so the reconciler can observe; the unified form does not
yet collect any credential material.

## Host prerequisites

The product owner's directive: "we also need to deliver the dependencies
since the hosting OS might not have them installed". The framework
lives at `computeza-driver-native::prerequisites`. Today's split:

**Bundled in pure Rust (no host dep):**

- Archive extraction -- zip, tar.gz, tar.xz (xz via `liblzma` with the
  `static` feature so virgin Linux hosts without `xz-utils` still work),
  raw binaries
- HTTP fetch -- `reqwest`
- SHA-256 verification -- `sha2`
- X.509 self-signed cert generation -- `rcgen` (kanidm TLS bootstrap).
  Used to require an `openssl` shell-out; the pure-Rust path means a
  virgin Linux host without the openssl CLI installed still works.
- Service registration -- `systemctl` shell wrapper (systemd is a
  baseline assumption on every supported distro)

**Host-installed (operator must provide):**

- *(empty)* -- as of the rcgen + bundled-cargo work, the install path
  has no remaining hard host prereqs on a virgin Linux. Future entries
  go here only when adding a dependency that genuinely cannot be
  auto-installed.

**Computeza-delivered (auto-installed on the host):**

- Rust toolchain (`prerequisites::ensure_rust_toolchain`) -- system-
  wide install for the `cargo install kanidmd --locked` step on the
  kanidm path. When `cargo` is missing from `$PATH`, the driver
  downloads the official `rustup-init` static binary from
  `static.rust-lang.org` and runs it with
  `CARGO_HOME=/var/lib/computeza/toolchain/rust/cargo`,
  `RUSTUP_HOME=/var/lib/computeza/toolchain/rust/rustup`,
  `--no-modify-path --profile minimal --default-toolchain stable -y`.
  After install the driver symlinks `cargo`, `rustc`, `rustup` onto
  `/usr/local/bin/` so the operator's shell (and any future component
  install) can find them on `$PATH` immediately. We deliberately do
  NOT modify shell rc files -- the `/usr/local/bin/` symlinks are
  sufficient and reverse cleanly. Subsequent installs that find
  `cargo` on `$PATH` skip the bootstrap entirely. ~500MB first-run,
  shared across all components / re-installs after that.
- Adoptium Temurin JRE 21 (`prerequisites::TEMURIN_JRE_21_X86_64_LINUX`)
  -- xtable install only. Drops into `<root_dir>/jre/` next to the
  runner JAR, never touches system PATH (it's internal plumbing for
  one component, not a tool the operator runs directly), removed by
  uninstall. Designed but not yet wired -- blocked on the xtable
  runner-JAR distribution question (see below).

**Uninstall semantics for shared toolchains:** removing one component
does NOT remove the shared Rust toolchain at
`/var/lib/computeza/toolchain/rust/` -- another component may need it,
and re-installing is expensive. A future v0.1 "purge toolchains"
action will tear it down cleanly; today operators who want to fully
clean can `rm -rf /var/lib/computeza/toolchain/` and
`rm /usr/local/bin/{cargo,rustc,rustup}`.

Adding a new host dep:

1. Add a `SystemCommand` entry to `prerequisites::SYSTEM_COMMANDS`.
2. Have the install wizard call `prerequisites::which_on_path(name)` in
   its form handler and surface a banner with `install_hint` when the
   command is missing.
3. Do NOT auto-`apt-get`/`dnf`/`pacman` install -- v0.0.x principle is
   "detect + surface", not "detect + auto install on the operator's
   box". Auto-installing into an isolated `<root_dir>/<tool>/` is fine
   (that's the Temurin pattern).

## xtable: open infrastructure question

xtable is the 11th managed component. As of May 2026 it cannot be
shipped from this repo because Apache distributes the runnable artifact
in one of three forms, none of which is a turnkey download:

1. **Apache dist** (`dist.apache.org/repos/dist/release/incubator/xtable/`)
   -- source-only tarball (`apache-xtable-0.3.0-incubating.src.tgz`).
   Requires JDK + Maven + a multi-minute `mvn package` build at install
   time.
2. **GitHub Releases** (`apache/incubator-xtable`) -- zero asset files.
3. **Maven Central** (`org.apache.xtable/xtable-service/0.3.0-incubating`)
   -- 28 KB thin JAR. Running it requires Maven to resolve and download
   ~50-100 transitive deps at install time.

Realistic paths for v0.1+:

- (a) Computeza-side build pipeline that produces a fat JAR and hosts
  it on a Computeza CDN. The driver then becomes a normal
  `Bundle { kind: TarGz, ... }` + the Temurin JRE bootstrap. Cleanest
  but requires release infrastructure that doesn't exist yet.
- (b) Install-time Maven resolve. Driver invokes Maven to resolve
  `xtable-service` + transitive deps into `<root_dir>/lib/`, registers
  a systemd unit running `java -cp <root>/lib/*:<root>/jre/...`.
  Workable but requires Maven on the host AND the JRE bootstrap.
- (c) Install-time source build. Driver clones the Apache source
  tarball, builds with `mvn package`, uses the resulting fat JAR.
  Slowest path; useful only if the operator wants to track upstream
  closely.

Until one of (a)/(b)/(c) lands, xtable stays at `available: false` on
the install hub and `/install/xtable` renders the CLI-explainer page.
The reconciler crate is fully implemented and ready for the day the
runner JAR is reachable.
