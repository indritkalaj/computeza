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

**v0.0.x is Linux-only for the data-plane install path.** Every one of
the 11 managed components (PostgreSQL, Kanidm, Garage, OpenFGA, ...)
installs only on systemd-based Linux on x86_64. macOS + Windows native
install drivers move to v0.1+; the operator console's install hub
explicitly refuses to run an install when it detects a non-Linux host
(via `computeza-driver-native::os_detect`).

The macOS + Windows PostgreSQL driver modules under
`crates/computeza-driver-native/src/{macos,windows}/postgres.rs` exist
as reference code from an earlier iteration but are no longer reachable
through the wizard. They are not extended for new components.

The operator console itself (`computeza serve`) runs on any OS that
builds Rust. You can run the web UI on Windows pointing at remote Linux
hosts that own the actual installs once the multi-host install path
lands in v0.1+; today the install actions are local-only and need a
Linux host. Spec section 10 documents the multi-OS roadmap.

**Supported Linux distros:** Ubuntu 22.04 LTS+, Debian 12+, Fedora 38+,
RHEL / CentOS Stream 9, Rocky Linux 9, AlmaLinux 9, OpenSUSE Leap 15.6
or Tumbleweed, SLES 15, Arch Linux (rolling), Manjaro (rolling).
**Not supported:** Alpine (musl + OpenRC), Gentoo (OpenRC default),
container-image base distros without an init.

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
