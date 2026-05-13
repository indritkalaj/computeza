# Computeza

> An all-Rust open lakehouse control plane -- single binary installer, operator
> console, and configurator. Sells software per-user, never compute or storage.

Computeza installs a complete production-grade data platform built from
best-of-breed open-source Rust components onto bare metal, on-premises VMs,
private cloud, public cloud, or hybrid. Customers own their compute and
storage; Computeza manages everything that runs on them -- instances,
clusters, users, permissions, pipelines, AI workloads -- entirely from a
GUI-first operator console with full GitOps / IaC equivalence.

## Status

**Pre-alpha.** Workspace scaffolding only. Architecture and product scope
are governed by `docs/Architecture-and-Product-Specification-v1.5.pdf`
(referenced as "the spec" in source comments). v1.0 GA target: **Q2 2027**
per spec section 13.

## Platform support

**v0.0.x supports Ubuntu Linux x86_64 only.** This is a hard
constraint of the current release, not a recommendation: Databend's
official binary release (a glibc-linked tarball at
`github.com/databendlabs/databend`) has been verified only against
Ubuntu's glibc + systemd userspace, and the other 10 managed
components are end-to-end tested only on the same target. We will
broaden the matrix in v0.1+ once Databend's release-engineering
catches up (or once we ship a source-build fallback for Databend
analogous to what kanidm and garage already use).

**Supported today:**

- Ubuntu 22.04 LTS (minimum) on x86_64
- Ubuntu 24.04 LTS (recommended) on x86_64
- WSL2 (Ubuntu) with `systemd=true` enabled in `/etc/wsl.conf`

**Best-effort, unverified end-to-end:** Debian 12+, Fedora 39+,
RHEL / Rocky / AlmaLinux 9+, openSUSE, Arch. Ten of the eleven
components install on these; Databend is the binding constraint.

**Not supported in v0.0.x:** macOS, Windows, ARM64, Alpine
(musl libc), Gentoo, any container-image base without systemd
as PID 1.

The macOS + Windows PostgreSQL driver modules under
`crates/computeza-driver-native/src/{macos,windows}/postgres.rs`
exist as reference code from an earlier iteration but are no
longer reachable through the wizard. They are not extended for
new components.

The operator console itself (`computeza serve`) compiles and
runs on any OS that builds Rust. You can run the web UI on
Windows pointing at remote Ubuntu hosts that own the actual
installs once the multi-host install path lands in v0.1+; today
the install actions are local-only and need an Ubuntu host. Spec
section 10 documents the multi-OS roadmap.

## Core principles

These are non-negotiable. They drive every design decision in the codebase.

- **Single binary, three things.** One Rust binary acts as installer (first
  run), operator console (day-2 web UI), and configurator (declarative state
  engine with GitOps mode). See spec section 2.1.
- **GUI-first.** Every administrative operation -- installing components,
  managing clusters, creating users, granting permissions, deploying
  pipelines, configuring AI agents -- must be reachable from the web console.
  CLI and YAML are escape hatches for power users and CI, not the primary
  interface.
- **Native install everywhere.** Linux (systemd), macOS (launchd), Windows
  (Services). No Docker, no Kubernetes, no container runtime required for
  v1.0. Kubernetes and cloud drivers are deferred to v1.2. See spec section 10.
- **No hardcoded user-facing strings.** All text routes through the
  [`computeza-i18n`](crates/computeza-i18n) crate (Fluent / `.ftl` resource
  bundles). English is the only locale today; additional locales drop in
  without code changes. Pull requests that hardcode user-visible English are
  not accepted.
- **Per-user pricing only.** No metering of compute, queries, scans, bytes,
  or anything else customers already pay infrastructure providers for. See
  spec section 11.
- **The control plane manages the lakehouse; the control plane is not the
  lakehouse.** If Computeza stops, the data plane keeps serving. See spec section 3.1.

## Workspace layout

The crates follow the four tiers from spec section 3.2.

```
crates/
+-- computeza/                     # the single binary entry point
|
+-- computeza-core/                # Tier 1 -- Core Engine
+-- computeza-state/               #   persistence over SQLite/Postgres
+-- computeza-audit/               #   append-only signed audit log
+-- computeza-secrets/             #   encrypted secret storage
+-- computeza-driver/              #   Driver trait
+-- computeza-tenancy/             #   workspace isolation, quotas
+-- computeza-pipelines/           #   pipeline definition + Restate compile
|
+-- computeza-driver-native/       # Tier 2 -- Drivers (v1.0: native only)
|
+-- computeza-reconciler-kanidm/   # Tier 3 -- Component Reconcilers
+-- computeza-reconciler-garage/
+-- computeza-reconciler-lakekeeper/
+-- computeza-reconciler-xtable/
+-- computeza-reconciler-databend/
+-- computeza-reconciler-qdrant/
+-- computeza-reconciler-restate/
+-- computeza-reconciler-greptime/
+-- computeza-reconciler-grafana/
+-- computeza-reconciler-postgres/
+-- computeza-reconciler-openfga/
|
+-- computeza-i18n/                # Tier 4 -- Web Console (i18n is shared)
+-- computeza-ui-server/           #   Leptos SSR entry point
+-- computeza-ui-components/       #   design system library
+-- computeza-ui-pages/            #   page modules (Identity, Catalogs, ...)
+-- computeza-ui-pipelines/        #   drag-and-drop pipeline canvas
+-- computeza-ui-themes/           #   brand themes, white-label
```

Additional reconcilers introduced in spec v1.5 (MLflow, Model Gateway,
vLLM/TGI, Apache AGE) will be added when the AI Workspace milestones land --
see spec section 6 and section 13.2.

## Development

```sh
# Build everything
cargo build --workspace

# Lint
cargo clippy --workspace --all-targets

# Run tests
cargo test --workspace

# Run the CLI
cargo run --bin computeza -- --help
```

The `rust-toolchain.toml` pins the Rust version; `rustup` will install it
automatically on first build.

## Roadmap (selected deferred work)

Tracked in detail under `AGENTS.md` ("Deferred work / open TODOs").
Headline items not yet in v0.0.x:

- **Scheduled secrets auto-rotation + PGP-encrypted notification.**
  Manual rotation works today via `/admin/secrets/{name}/rotate`
  (and now applies the new value to postgres). Auto-rotate ships in
  three pieces -- per-secret policy editor, hourly scheduler that
  reuses the manual-rotate code path, and PGP-encrypted-over-SMTP
  mail dispatch to a configurable recipient list. Pieces 1+2 are a
  contained v0.1 milestone; piece 3 (with `sequoia-openpgp` +
  per-operator key UI) gets its own design pass.
- **Apply-admin-password for kanidm + grafana.** Postgres works
  end-to-end; kanidm + grafana store generated passwords in the
  vault but don't push them to the running component yet (each
  needs its own mechanism -- `kanidmd recover_account` for kanidm,
  `/api/admin/users/{id}/password` for grafana).
- **XTable runtime invocation.** The install pipeline provisions
  JRE + JAR + systemd unit, but ExecStart is `/bin/true` for
  v0.0.x. Upstream JDK plugin-pin conflicts block a runnable build
  today; full analysis in
  [`crates/computeza-driver-native/src/linux/xtable.rs`](crates/computeza-driver-native/src/linux/xtable.rs).
- **Cross-platform expansion.** macOS + Windows native drivers,
  non-Ubuntu Linux distros, Kubernetes driver -- all v0.1+ once
  Databend's release engineering catches up or we ship a
  source-build fallback.

## License

See [LICENSE.md](LICENSE.md). Computeza is commercial software licensed
per-user under three retail tiers (Team, Business, Sovereign) plus a
Provider channel program -- see spec section 11. The repository is not licensed for
redistribution.

## Spec reference

The canonical source of architectural truth is the v1.5 Architecture &
Product Specification (104 pages, April 2026). When source comments refer
to a section number, that document is what they mean. The spec was written
before the product name was decided; in the spec it is called "the product"
or "the platform". In code we always use **Computeza**.
