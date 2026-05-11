# Architecture (working overview)

This is a working overview that mirrors spec section 3 and links each section to
the crate that owns it. The spec PDF remains the canonical reference; this
document grows as the codebase fills in.

## Control plane vs data plane

Per spec section 3.1:

- The **control plane** is a single Rust binary (`computeza`). It serves
  the web UI, the REST/gRPC APIs, and runs the reconciliation loop. It
  persists its desired-state metadata in SQLite (single-node) or
  PostgreSQL (HA). It owns no customer data.
- The **data plane** is the collection of managed components. They are
  deployed and configured by the control plane but operate independently.
  If the control plane stops, the data plane keeps serving.

This separation is the lock-in escape hatch (spec section 1.2 Design Principle):
if Computeza is uninstalled, every component remains operable through its
native interfaces.

## Workspace tiers

Crates are organised into four tiers per spec section 3.2.

### Tier 1 -- Core engine

Foundational crates every other crate depends on. Domain types, persistence,
audit, secrets, drivers, tenancy, pipelines.

| Crate                 | Owns                                                  | Spec  |
| --------------------- | ----------------------------------------------------- | ----- |
| `computeza-core`      | Resource / Reconciler / Driver / Health / Error traits | section 3.3, section 3.4, section 3.5 |
| `computeza-state`     | SQLite/Postgres persistence (SQLx)                    | section 3.1  |
| `computeza-audit`     | Append-only signed audit log (Ed25519)                | section 3.5, section 4.5 |
| `computeza-secrets`   | AES-256-GCM secret storage, CMK integration           | section 3.2, section 8.4 |
| `computeza-driver`    | Driver registry, re-exports `Driver` trait            | section 3.4  |
| `computeza-tenancy`   | Workspace isolation, quotas, per-tenant metering      | section 3.6  |
| `computeza-pipelines` | Pipeline YAML schema, Restate compilation             | section 5    |

### Tier 2 -- Drivers

The driver layer abstracts the deployment target. v1.0 ships only the
native driver; Kubernetes and cloud drivers are deferred to v1.2.

| Crate                      | Target                                                  | Status     |
| -------------------------- | ------------------------------------------------------- | ---------- |
| `computeza-driver-native`  | systemd / launchd / Windows Services                    | v1.0       |
| `computeza-driver-k8s`     | Kubernetes via kube-rs                                  | v1.2 (TODO) |
| `computeza-driver-cloud-*` | AWS / Azure / GCP via OpenTofu wrappers                 | v1.2 (TODO) |

### Tier 3 -- Component reconcilers

One crate per managed component. Each implements `Reconciler` against the
component's native API.

| Crate                              | Component  | Spec |
| ---------------------------------- | ---------- | ---- |
| `computeza-reconciler-kanidm`      | Kanidm     | section 7.1 |
| `computeza-reconciler-garage`      | Garage     | section 7.2 |
| `computeza-reconciler-lakekeeper`  | Lakekeeper | section 7.4 |
| `computeza-reconciler-xtable`      | Apache XTable (sidecar; bundled JRE) | section 7.5 |
| `computeza-reconciler-databend`    | Databend   | section 7.6 |
| `computeza-reconciler-qdrant`      | Qdrant     | section 7.8 |
| `computeza-reconciler-restate`     | Restate    | section 7.9 |
| `computeza-reconciler-greptime`    | GreptimeDB | section 7.10 |
| `computeza-reconciler-grafana`     | Grafana    | section 7.11 |
| `computeza-reconciler-postgres`    | PostgreSQL | section 7.13 |
| `computeza-reconciler-openfga`     | OpenFGA    | section 7.12 |

v1.5 introduces four additional reconcilers (Apache AGE, MLflow, Model
Gateway, vLLM/TGI) for the AI Workspace per spec section 7.14-section 7.17. They are
not yet scaffolded; they land alongside the AI Workspace milestones in
section 13.2.

### Tier 4 -- Web console

Leptos SSR application and supporting libraries.

| Crate                     | Owns                                       | Spec |
| ------------------------- | ------------------------------------------ | ---- |
| `computeza-i18n`          | Fluent (.ftl) localizer; shared by CLI+UI  | section 4.1 |
| `computeza-ui-server`     | Leptos SSR, axum routing, WebSocket events | section 4.1 |
| `computeza-ui-components` | 47-component design system                 | section 3.2 |
| `computeza-ui-pages`      | Page modules (Identity, Catalogs, ...)       | section 4.2 |
| `computeza-ui-pipelines`  | Drag-and-drop pipeline canvas              | section 5   |
| `computeza-ui-themes`     | Brand themes, white-label                  | section 11.6 |

### The binary

| Crate       | Owns                                                       |
| ----------- | ---------------------------------------------------------- |
| `computeza` | Single binary entry point: installer + console + configurator |

## The reconciler pattern

Every reconciler is an idempotent loop: `observe -> plan -> apply -> health`.
Spec section 3.3 has the trait pseudocode; `crates/computeza-core/src/reconciler.rs`
is the Rust translation. Reconciliation runs every 30 seconds for health
observation and on-demand whenever desired state changes. Failed
reconciliations enter retry-with-backoff and surface in the UI as a drift
indicator.

## The driver pattern

Drivers abstract the deployment target. The same reconciler logic
produces a Garage cluster on native Linux, native macOS, native Windows,
on Kubernetes, or on AWS EC2 -- only the driver differs. The `Driver`
trait is intentionally narrow (spec section 3.4); see
`crates/computeza-core/src/driver.rs`.

## See also

- Spec section 3 -- full architecture
- Spec section 10 -- cross-platform packaging (the implementation that backs `driver-native`)
- [`docs/i18n.md`](./i18n.md) -- internationalisation rules
