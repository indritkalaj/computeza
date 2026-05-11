# Computeza -- English (en) UI message bundle.
#
# Strings rendered by the operator console (Leptos SSR). Subsystem-specific
# bundles will appear alongside this one (e.g. ui-pipelines.ftl, ui-audit.ftl)
# as those sections of the console come online.

# --- Top-level shell ---

ui-app-title          = Computeza
ui-app-tagline        = Open lakehouse control plane

# --- Index page (the "Hello, GUI" placeholder) ---

ui-welcome-title      = Welcome to Computeza
ui-welcome-subtitle   = Your operator console is online.
ui-welcome-status     = Pre-alpha -- the operator console is a placeholder while the rest of the platform is being built.
ui-welcome-spec       = See the Architecture & Product Specification v1.5 for the full feature plan.

# --- Health check ---

ui-healthz-ok         = ok

# --- Footer ---

ui-footer-version     = Version

# --- Navigation ---

ui-nav-components     = Components

# --- Components page ---

ui-components-title       = Managed components
ui-components-intro       = The data-plane components Computeza installs and reconciles. Per spec section 2.2.
ui-components-col-name    = Name
ui-components-col-kind    = Kind
ui-components-col-role    = Role

# Per spec section 2.2 component table
component-kanidm-name     = Kanidm
component-kanidm-role     = OIDC/OAuth2 IdP, passkeys, RADIUS

component-garage-name     = Garage
component-garage-role     = S3-compatible, geo-distributed object storage

component-lakekeeper-name = Lakekeeper
component-lakekeeper-role = Iceberg REST catalog with Generic Tables

component-xtable-name     = Apache XTable
component-xtable-role     = Iceberg <-> Delta <-> Hudi metadata sync (Java sidecar)

component-databend-name   = Databend
component-databend-role   = Columnar SQL, vector, full-text search, geospatial

component-qdrant-name     = Qdrant
component-qdrant-role     = Production RAG vector retrieval API

component-restate-name    = Restate
component-restate-role    = Durable execution orchestrator + agent runtime

component-greptime-name   = GreptimeDB
component-greptime-role   = Unified metrics, logs, traces

component-grafana-name    = Grafana
component-grafana-role    = BI and visualisation dashboards

component-postgres-name   = PostgreSQL
component-postgres-role   = Catalog, IdP, MLflow backing store

component-openfga-name    = OpenFGA
component-openfga-role    = Fine-grained authorization (Zanzibar)
