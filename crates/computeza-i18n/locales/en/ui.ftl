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
ui-welcome-lead       = Welcome to
ui-welcome-subtitle   = Your operator console is online.
ui-home-surfaces      = Operator surfaces
ui-home-pre-alpha     = pre-alpha
ui-welcome-status     = Pre-alpha -- the operator console is a placeholder while the rest of the platform is being built.
ui-welcome-spec       = See the Architecture & Product Specification v1.5 for the full feature plan.

# --- Home dashboard cards ---

ui-home-card-components-title = Managed components
ui-home-card-components-body  = The 11 data-plane components Computeza manages. Pulled from spec section 2.2.
ui-home-card-install-title    = Install a component
ui-home-card-install-body     = Run the GUI-equivalent of `computeza install <component>`. v0.0.x ships PostgreSQL; the rest follow.
ui-home-card-status-title     = Reconciler status
ui-home-card-status-body      = Live observations from every reconciler bound to this server, with per-row drill-down.
ui-home-card-state-title      = Metadata store
ui-home-card-state-body       = Resource counts per kind, JSON-shaped for programmatic callers.
ui-home-store-empty           = No resources registered.
ui-home-store-missing         = No metadata store attached on this server.

# --- Metadata store page ---

ui-state-title         = Metadata store
ui-state-intro         = The raw shape of the SQLite-backed metadata graph that drives the operator console. Each row is one resource kind and how many instances of it are registered.
ui-state-col-kind      = Kind
ui-state-col-count     = Instances
ui-state-store-missing = No metadata store is attached to this server. Start it with `computeza serve` to populate this view.
ui-state-store-empty   = The store is attached but nothing has been registered yet. Run the install wizard or apply a spec to populate it.
ui-state-view-json     = View raw JSON

# --- Health check ---

ui-healthz-ok         = ok

# --- Footer ---

ui-footer-version     = Version

# --- Navigation ---

ui-nav-components     = Components
ui-nav-install        = Install
ui-nav-status         = Status
ui-nav-state          = Metadata store

# --- Status page ---

ui-status-title         = Reconciler status
ui-status-intro         = Live observations from every reconciler currently bound to this server. Each row is one resource instance and its most recent status snapshot (spec section 4.4 drift surface).
ui-status-col-kind      = Kind
ui-status-col-name      = Name
ui-status-col-version   = Server version
ui-status-col-observed  = Last observed
ui-status-col-state     = State
ui-status-state-ok      = Observing
ui-status-state-failed  = Failed
ui-status-state-unknown = Unknown
ui-status-empty         = No resources have been observed yet. Bind a reconciler with `.with_state(store, instance_name)` and run it to populate this view.
ui-status-store-missing = No metadata store is attached to this server. Start it with `computeza serve` (the binary wires a SqliteStore automatically) to see live reconciler state here.

# --- Resource detail page ---

ui-resource-title         = Resource detail
ui-resource-not-found     = This resource is not in the metadata store. It may have been deleted, or the reconciler that owns it hasn't run yet.
ui-resource-back          = Back to status
ui-resource-uuid          = UUID
ui-resource-revision      = Revision
ui-resource-created-at    = Created at
ui-resource-updated-at    = Updated at
ui-resource-workspace     = Workspace
ui-resource-spec-heading  = Desired spec
ui-resource-status-heading = Observed status
ui-resource-no-status     = No status snapshot recorded yet.
ui-resource-store-missing = This page needs a metadata store. Start the server with `computeza serve` to attach one.
ui-resource-delete-button = Delete resource
ui-resource-delete-confirm = This drops the resource from the metadata store. The on-disk service (if any) is not touched.
ui-resource-deleted       = Resource removed from the metadata store.
ui-resource-delete-failed = Could not delete resource:

# --- Install wizard ---

ui-install-title         = Install a component
ui-install-intro         = `computeza install <component>` lays down a native OS service. The same install path runs from the CLI -- this page is the GUI-equivalent per spec section 2.1 / 4.2.
ui-install-hub-title     = Install Computeza
ui-install-hub-intro     = Computeza is a single product made of 11 data-plane components -- the lakehouse only works when each one is in place. Configure every service below in one pass, then click Install at the bottom of the page to lay them all down in dependency order. Each card collects the service name, network port, data directory, version pin, and (in v0.1+) the credentials and identity-federation binding for that component. v0.0.x ships 10 of the 11 components; xtable is blocked on its upstream runner-JAR distribution.

# --- Unified install (whole-stack) page ---
#
# Strings used by the per-component cards inside the unified /install
# page. One submit button at the bottom drives every install in the
# canonical dependency order.

ui-install-card-service-config = Service configuration
ui-install-card-identity       = Identity and access
ui-install-card-identity-help  = Service account, initial admin credentials, group permissions, and upstream IdP federation (Entra ID, AWS IAM, GCP IAM, on-prem LDAP / Kerberos) configure here in v0.1+. Today every component installs against a loopback-trust auth surface so the operator console can observe it; bind credentials and federation in v0.1 once `computeza-secrets` is wired into the install flow.
ui-install-card-identity-v01   = Configurable in v0.1+
ui-install-all-button          = Install all components
ui-install-all-helper          = Clicking Install lays down each component in dependency order (postgres -> openfga -> kanidm -> garage -> qdrant -> lakekeeper -> greptime -> grafana -> restate -> databend). Progress for the running component shows on the next page; a failure stops the chain and leaves earlier components installed.
ui-install-component-unavailable = Pinned in tree, not yet shippable on Linux. Skipped automatically.

# --- Install progress page (per-component checklist) ---

ui-install-progress-title-multi  = Installing Computeza
ui-install-progress-intro-multi  = Each component below is laid down in order. You can leave this page open; it polls the server every half second and survives browser refresh.
ui-install-progress-title-single = Installing component
ui-install-progress-intro-single = Computeza is preparing the service. You can leave this page open; it polls the server every half second.
ui-install-progress-components   = Components
ui-install-progress-state-pending = Pending
ui-install-progress-state-running = Running
ui-install-progress-state-done    = Done
ui-install-progress-state-failed  = Failed

ui-platform-banner-supported   = Detected:
ui-platform-banner-unsupported = Host not supported
ui-platform-supported-title    = Supported platforms
ui-platform-supported-intro    = Computeza v0.0.x installs data-plane components on systemd-based Linux only. The operator console (this web UI) runs anywhere; the install actions need a Linux host. Once the multi-host install path lands in v0.1+ you'll be able to run the console here and point it at remote Linux targets.
ui-platform-supported-distros  = Ubuntu 22.04 LTS or newer, Debian 12 or newer, Fedora 38 or newer, RHEL / CentOS Stream 9, Rocky Linux 9, AlmaLinux 9, OpenSUSE Leap 15.6 or Tumbleweed, SLES 15, Arch Linux (rolling), Manjaro (rolling). Architecture: x86_64.

# --- Prerequisite banner ---
#
# Shown above an install wizard when a host command the driver shells
# out to is missing from $PATH. The component-specific list of required
# commands lives in the component's wizard handler; the strings here are
# component-agnostic.

ui-prerequisite-banner-title        = Host command missing
ui-prerequisite-banner-intro        = This install shells out to commands that are not on `$PATH`. Install them on the host and refresh this page. The install button will work once every command below resolves.
ui-prerequisite-banner-needed-for   = Needed for
ui-prerequisite-banner-install-hint = Install with
ui-install-status-available  = Available
ui-install-status-planned    = Planned
ui-install-coming-soon-title = Install from the CLI
ui-install-coming-soon-body  = This component's native install path is wired in `computeza-driver-native` for Linux (download + systemd unit + start). The per-component web wizard lands in a follow-up commit. Today: run `computeza install <slug>` from the CLI on a Linux host. Windows + macOS driver variants are also pending; postgres is the only component with full multi-OS coverage so far. The reconciler is already wired to observe any running instance once its spec is in the metadata store.
ui-install-coming-soon-back  = Back to install hub
ui-install-target-label  = Component
ui-install-postgres      = PostgreSQL (spec section 7.13)
ui-install-button        = Install
ui-install-requires-root = Note: native install needs root / Administrator privileges (writes /etc/systemd/system, /Library/LaunchDaemons, or HKLM Services). If you started the operator console without elevation the install POST will fail with a clear permission error -- re-run `computeza serve` as root / via UAC and retry.

ui-install-detected-title    = Already on this host
ui-install-detected-empty    = No PostgreSQL installs detected.
ui-install-detected-hint     = The form below is pre-filled with values that don't collide with what's already installed. Tweak any of them if you need a different layout.
ui-install-version-label     = Version
ui-install-version-help      = Latest stable is recommended. The previous-major line is offered for operators who need to pin against an older release.
ui-install-version-latest    = (latest)
ui-install-version-host      = (host-installed)
ui-install-port-label        = TCP port
ui-install-port-help         = The address PostgreSQL listens on. Default 5432. Pick another if you already have a PostgreSQL on 5432.
ui-install-data-dir-label    = Data directory
ui-install-data-dir-help     = Where the cluster files live. Leave blank for the default under %PROGRAMDATA%\Computeza\postgres (Windows) / /var/lib/computeza/postgres (Linux) / /Library/Application Support/Computeza/postgres (macOS).
ui-install-service-name-label = Service name
ui-install-service-name-help  = Identifier registered with the OS service manager. Override only if `computeza-postgres` collides with another service you already have.
ui-install-advanced          = Advanced options
ui-install-already-installed = Already installed? Uninstall first to start fresh.

ui-install-kanidm-title  = Install Kanidm
ui-install-kanidm-intro  = Kanidm is the identity provider Computeza uses for SSO, OAuth2, passkeys, and RADIUS. The install compiles `kanidmd` from crates.io (10-15 min), generates a self-signed TLS cert, writes a minimal `server.toml`, and registers a systemd unit. After install run `kanidmd recover_account admin` to bootstrap the initial admin password.

ui-uninstall-kanidm-title = Uninstall Kanidm
ui-uninstall-kanidm-intro = Roll back the Kanidm install: stop the service, remove the OS service unit, delete the data directory under the install root, and drop kanidm-instance/local from the metadata store. The cached binary bundle is preserved so re-install is fast.

ui-install-garage-title  = Install Garage
ui-install-garage-intro  = Garage is the S3-compatible, geo-distributed object store. Install drops the prebuilt binary from the deuxfleurs CDN, writes a single-node `garage.toml` (replication_factor = 1, sqlite metadata), and registers a systemd unit. Adjust replication + multi-node layout via the admin API after first boot.
ui-uninstall-garage-title = Uninstall Garage
ui-uninstall-garage-intro = Roll back the Garage install: stop the service, remove the systemd unit, delete the data directory, drop garage-instance/local from the metadata store. The cached binary is preserved.

ui-install-openfga-title  = Install OpenFGA
ui-install-openfga-intro  = OpenFGA is the fine-grained authorization service (Zanzibar-style). Install downloads the binary from the upstream GitHub release, runs it with in-memory storage on the chosen port, and registers a systemd unit. Switch to Postgres storage by editing the unit's ExecStart args.
ui-uninstall-openfga-title = Uninstall OpenFGA
ui-uninstall-openfga-intro = Roll back the OpenFGA install: stop the service, remove the systemd unit, drop openfga-instance/local from the metadata store. In-memory storage means there is no data dir to delete.

ui-install-qdrant-title  = Install Qdrant
ui-install-qdrant-intro  = Qdrant is the production vector retrieval API. Install downloads the binary, writes a minimal `config.yaml`, registers a systemd unit, and binds REST on the chosen port + gRPC on port+1.
ui-uninstall-qdrant-title = Uninstall Qdrant
ui-uninstall-qdrant-intro = Roll back the Qdrant install: stop the service, remove the systemd unit, delete the data directory (vector storage). Drops qdrant-instance/local from the metadata store.

ui-install-greptime-title  = Install GreptimeDB
ui-install-greptime-intro  = GreptimeDB is the unified observability database (metrics, logs, traces). Install downloads the binary, registers a systemd unit running `greptime standalone start`, binds HTTP on the chosen port.
ui-uninstall-greptime-title = Uninstall GreptimeDB
ui-uninstall-greptime-intro = Roll back the GreptimeDB install: stop the service, remove the systemd unit, delete the data directory. Drops greptime-instance/local from the metadata store.

ui-install-lakekeeper-title  = Install Lakekeeper
ui-install-lakekeeper-intro  = Lakekeeper is the Iceberg REST catalog with Generic Tables support. Install downloads the binary, registers a systemd unit. Note: Lakekeeper needs a PostgreSQL backing store -- install postgres-instance first and configure the connection in the systemd unit's environment block.
ui-uninstall-lakekeeper-title = Uninstall Lakekeeper
ui-uninstall-lakekeeper-intro = Roll back the Lakekeeper install: stop the service, remove the systemd unit, drop lakekeeper-instance/local from the metadata store.

ui-install-databend-title  = Install Databend
ui-install-databend-intro  = Databend is the columnar SQL + vector + full-text + geospatial engine. Install downloads the binary from the databendlabs GitHub release, writes a minimal `databend-query.toml` (fs storage backend), and registers a systemd unit.
ui-uninstall-databend-title = Uninstall Databend
ui-uninstall-databend-intro = Roll back the Databend install: stop the service, remove the systemd unit, delete the data directory. Drops databend-instance/local from the metadata store.

ui-install-grafana-title  = Install Grafana
ui-install-grafana-intro  = Grafana is the BI and visualization layer. Install downloads the binary from dl.grafana.com, registers a systemd unit. Default admin login is `admin / admin` -- change it immediately after first login.
ui-uninstall-grafana-title = Uninstall Grafana
ui-uninstall-grafana-intro = Roll back the Grafana install: stop the service, remove the systemd unit, delete the data directory (dashboards, datasources). Drops grafana-instance/local from the metadata store.

ui-install-restate-title  = Install Restate
ui-install-restate-intro  = Restate is the durable-execution engine for stateful workflows + invocations. Install downloads the `restate-server` binary from the GitHub `.tar.xz` release (liblzma is statically linked so a virgin Linux host without xz-utils still works) and registers a systemd unit on the chosen ingress port.
ui-uninstall-restate-title = Uninstall Restate
ui-uninstall-restate-intro = Roll back the Restate install: stop the service, remove the systemd unit, delete the data directory (durable state). Drops restate-instance/local from the metadata store.

ui-uninstall-title    = Uninstall PostgreSQL
ui-uninstall-intro    = This rolls back the install: stops the Windows service, deletes the data directory, removes the psql shim from PATH, and drops postgres-instance/local from the metadata store. The cached binary bundle is preserved so re-install is fast.
ui-uninstall-confirm  = This deletes the cluster's data directory. Any databases inside will be permanently lost. Make a backup first if you care about the data.
ui-uninstall-button   = Confirm uninstall
ui-uninstall-cancel   = Cancel
ui-uninstall-success  = Uninstall completed.
ui-uninstall-failed   = Uninstall failed.

ui-install-result-title     = Install result
ui-install-result-success   = Install completed.
ui-install-result-failed    = Install failed.
ui-install-result-back      = Back to install wizard

# --- Components page ---

ui-components-title       = Managed components
ui-components-intro       = The data-plane components Computeza installs and reconciles. Per spec section 2.2.
ui-components-col-name    = Name
ui-components-col-kind    = Kind
ui-components-col-role    = Role
ui-components-col-license = License
ui-components-license-intro = License badge colour reflects copyleft strength: green = permissive (Apache / MIT / PostgreSQL), lavender = weak copyleft (MPL), peach = restrictive (BSL / Elastic-2.0), coral = strong copyleft (AGPL). The Computeza control plane itself is proprietary; managed components retain their upstream licenses (see docs/sbom.md and docs/licensing.md for the full position).

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
