# Software Bill of Materials (SBOM)

The components Computeza installs and manages, with their licenses,
upstream sources, and risk classification. This document is the
authoritative reference for procurement, legal review, and
funding-due-diligence questions.

**Update this file** whenever a component is added, removed, or its
upstream license changes. Treat divergence between this file and the
actual installed components as a release-blocking bug.

Last updated: 2026-05-12.

## How to read the risk column

| Risk | What it means for resellers / enterprise customers |
|------|----|
| **Permissive** | MIT / BSD / Apache 2.0 / PostgreSQL License. No copyleft. Use freely; attribution required. |
| **Weak copyleft** | MPL-2.0 / LGPL. Per-file copyleft scope. Linking from proprietary code is fine; modifying the component's own files triggers source-disclosure of those modifications. |
| **Restrictive** | BSL / Elastic License v2 / SSPL. Permissive-style for most use but with anti-cloud / anti-resale clauses. Read the specific clauses; some explicitly block managed-service offerings. Most include a time-bomb to Apache 2.0 after 3-4 years. |
| **Strong copyleft (AGPL)** | AGPL-3.0. Source-disclosure obligation triggers when modified versions are exposed over a network (sec 13). **Unmodified** AGPL components run as separate processes alongside Computeza without forcing Computeza into AGPL -- this is "mere aggregation" under AGPLv3 sec 5. See `docs/licensing.md` for the full analysis. |

## Computeza itself

| Item | License | Notes |
|------|---------|-------|
| `computeza` (this repo) | LicenseRef-Proprietary | Commercial. Sold per-user under three retail tiers (Team, Business, Sovereign) plus a Provider channel program. See spec section 11. |

## Managed components (data plane)

| Slug | Component | License | Risk | Upstream |
|------|-----------|---------|------|----------|
| `postgres` | PostgreSQL | PostgreSQL License | Permissive | https://www.postgresql.org/ |
| `kanidm` | Kanidm | MPL-2.0 | Weak copyleft | https://github.com/kanidm/kanidm |
| `garage` | Garage | **AGPL-3.0** | Strong copyleft | https://garagehq.deuxfleurs.fr/ |
| `lakekeeper` | Lakekeeper | Apache-2.0 | Permissive | https://github.com/lakekeeper/lakekeeper |
| `xtable` | Apache XTable (incubating) | Apache-2.0 | Permissive | https://xtable.apache.org/ |
| `databend` | Databend | Elastic-2.0 / Apache-2.0 split | Restrictive | https://github.com/databendlabs/databend |
| `qdrant` | Qdrant | Apache-2.0 | Permissive | https://github.com/qdrant/qdrant |
| `restate` | Restate | BSL-1.1 (-> Apache-2.0 after 4 yrs) | Restrictive | https://github.com/restatedev/restate |
| `greptime` | GreptimeDB | Apache-2.0 | Permissive | https://github.com/GreptimeTeam/greptimedb |
| `grafana` | Grafana | **AGPL-3.0** (since v3.0) | Strong copyleft | https://github.com/grafana/grafana |
| `openfga` | OpenFGA | Apache-2.0 | Permissive | https://github.com/openfga/openfga |

## Workspace-internal Rust crates Computeza pulls in

These are the runtime dependencies the `computeza` binary itself
links against. All permissively licensed; this is a sanity table,
not legal advice. Run `cargo about generate --output sbom-crates.html`
(or the licensing tool of your choice) for a complete machine-
readable list at release time.

| Family | License range | Notes |
|--------|---------------|-------|
| `tokio`, `axum`, `tower`, `hyper`, `tower-http` | MIT | Async runtime + HTTP stack |
| `serde`, `serde_json` | MIT OR Apache-2.0 | Serialization |
| `sqlx`, `sqlx-sqlite`, `sqlx-postgres` | MIT OR Apache-2.0 | SQL driver |
| `clap` | MIT OR Apache-2.0 | CLI parser |
| `tracing`, `tracing-subscriber` | MIT | Structured logging |
| `ed25519-dalek`, `aes-gcm`, `argon2`, `blake3`, `sha2` | MIT OR Apache-2.0 / BSD | Crypto primitives |
| `reqwest`, `rustls` | MIT OR Apache-2.0 / ISC | HTTPS client + TLS |
| `zip`, `tar`, `flate2`, `futures-util` | MIT OR Apache-2.0 | Archive extraction (driver-native) |
| `fluent-templates`, `unic-langid` | Apache-2.0 / MIT | i18n |
| `leptos` | MIT | (currently unused at runtime; reserved for v0.1 hydration) |
| `chrono`, `uuid`, `secrecy`, `zeroize`, `anyhow`, `thiserror` | MIT OR Apache-2.0 | Stdlib-adjacent |

No AGPL / GPL / SSPL / BSL crates appear in the workspace. Adding
any would require an explicit decision documented here.

## What the relationship is between Computeza and each AGPL component

**Garage** and **Grafana** are the two AGPL-3.0 components. The
control-plane / data-plane separation means:

- Computeza never links Garage or Grafana code into its own binary.
- The install path downloads the unmodified upstream binaries and
  registers them as separate OS services. Each runs in its own
  process with its own address space.
- The Computeza reconciler talks to Garage over Garage's HTTP admin
  API and to Grafana over Grafana's HTTP datasource / dashboard
  API. Both are documented external interfaces, not linker-level
  integrations.
- We do not modify either upstream's source.

Under AGPLv3 section 5 ("Conveying Modified Source Versions") and
section 13 (network-interaction clause), this aggregation pattern
keeps Computeza's license posture independent of either component's.
See `docs/licensing.md` for the full position paper.

## Considered alternatives (v0.1+ deferred)

For risk-averse customers who want to avoid AGPL components, the
v0.1+ roadmap should evaluate:

| Component | AGPL today | Permissive alternative under consideration |
|-----------|-----------|--------------------------------------------|
| Garage    | yes       | Ceph RGW (LGPL-2.1, more operational overhead); SeaweedFS (Apache-2.0). Trade-off: feature parity + replication story. |
| Grafana   | yes (v3+) | Apache SkyWalking UI (Apache-2.0, observability-focused); VictoriaMetrics UI (Apache-2.0, narrower scope). Trade-off: dashboard ecosystem. |

Neither alternative is wired up in v0.0.x; documented here so the
v0.1 cut has a clear starting point.

## CI hook (recommended, not yet wired)

Add a CI job that fails the build when:

- A new dependency lands with a license not on the workspace allow-list.
- A managed component's upstream changes license.
- The SBOM table in this file is older than the latest commit touching
  `crates/computeza-driver-native/src/linux/<component>.rs`.

Sketch: `cargo deny check licenses` + a custom script that greps for
`license =` in workspace `Cargo.toml`s and compares against the
allow-list in `deny.toml`. See https://github.com/EmbarkStudios/cargo-deny.
