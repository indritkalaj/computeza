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
