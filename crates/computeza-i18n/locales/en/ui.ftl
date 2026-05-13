# Computeza -- English (en) UI message bundle.
#
# Strings rendered by the operator console (Leptos SSR). Subsystem-specific
# bundles will appear alongside this one (e.g. ui-pipelines.ftl, ui-audit.ftl)
# as those sections of the console come online.

# --- Top-level shell ---

ui-app-title          = Computeza
ui-app-tagline        = Open lakehouse control plane

# --- Index page (the "Hello, GUI" placeholder, retained for legacy refs) ---

ui-welcome-title      = Welcome to Computeza
ui-welcome-lead       = Welcome to
ui-welcome-subtitle   = Your operator console is online.
ui-home-surfaces      = Operator surfaces
ui-home-pre-alpha     = pre-alpha
ui-welcome-status     = Pre-alpha -- the operator console is a placeholder while the rest of the platform is being built.
ui-welcome-spec       = See the Architecture & Product Specification v1.5 for the full feature plan.

# --- Public landing page (the marketing front door at /) ---
#
# Strings here power the public-facing landing page. Every visible
# word goes through the localizer so the page can be rebranded /
# translated per the reseller chain in v0.1+.

ui-landing-nav-signin    = Sign in
ui-landing-nav-docs      = Documentation
ui-landing-nav-github    = GitHub

ui-landing-hero-eyebrow   = Open lakehouse control plane
ui-landing-hero-title-pre = The Rust-native lakehouse,
ui-landing-hero-title-em  = in one self-hosted binary.
ui-landing-hero-subtitle  = Computeza installs, observes, and operates eleven open-source data components -- Postgres, Kanidm, Garage, Lakekeeper, Qdrant, Restate, Databend, GreptimeDB, Grafana, OpenFGA, Apache XTable -- through a single GUI-first console. No Docker. No Kubernetes. No SaaS lock-in. The data plane runs on your hardware; the operator console is the single pane of glass over it.
ui-landing-hero-cta-primary   = Sign in to your console
ui-landing-hero-cta-secondary = Browse the 11 components

ui-landing-stat-1-value = 11
ui-landing-stat-1-label = Managed components
ui-landing-stat-2-value = 0
ui-landing-stat-2-label = Docker containers
ui-landing-stat-3-value = 100%
ui-landing-stat-3-label = Rust toolchain
ui-landing-stat-4-value = MPL-2.0
ui-landing-stat-4-label = Open license

# --- What is Computeza ---

ui-landing-about-eyebrow   = What it is
ui-landing-about-title     = An operator console for the lakehouse stack you already understand.
ui-landing-about-subtitle  = Computeza is not a new database, not a new query engine, and not a new ML framework. It is also not a hosted service, a managed cloud, or a compute provider -- we ship software, you run it. Computeza is the missing operator layer that turns eleven best-in-class open-source projects into one production-shaped data plane: installed natively on your hardware, observed continuously, and torn down cleanly when you no longer need them.

# --- Features ---

ui-landing-features-eyebrow  = Capabilities
ui-landing-features-title    = Built for operators who deliver, not who tinker.
ui-landing-features-subtitle = Every surface below is GUI-first and CLI-equivalent. Anything you can click in the console, you can drive from `computeza`, your shell pipelines, or your CI.

ui-landing-feature-1-title = Unified one-click install
ui-landing-feature-1-body  = One Install button lays down every component in dependency order: postgres, openfga, kanidm, garage, qdrant, lakekeeper, greptime, grafana, restate, databend. Sequenced, idempotent, and resumable across browser refreshes.
ui-landing-feature-2-title = Encrypted secrets store
ui-landing-feature-2-body  = AES-256-GCM at rest, Argon2id-derived KEK, zero-on-drop in memory. Initial admin credentials generated post-install and surfaced one time; everything else lives under the rotate-secrets UI.
ui-landing-feature-3-title = Audit-grade event log
ui-landing-feature-3-body  = Every install, uninstall, and rotation routes through an ed25519-signed append-only audit log. Tamper-evident on disk, verifiable from the CLI, ready for compliance review without ad-hoc scraping.
ui-landing-feature-4-title = Live reconciler status
ui-landing-feature-4-body  = Each managed component is observed against its desired spec on a 30-second cadence. Drift surfaces immediately on the status page, with per-resource drill-down into the last successful observation and full spec history.
ui-landing-feature-5-title = Native OS services
ui-landing-feature-5-body  = systemd-managed services on Ubuntu Linux. No container runtime required, no orchestrator to babysit, no inner network to debug -- the installed binary is the daemon your `ps` already understands. v0.1+ broadens the platform matrix to Debian / Fedora / RHEL + macOS launchd + Windows Services; v0.0.x is Ubuntu-only because Databend's binary release is verified only there.
ui-landing-feature-6-title = Single-binary, single-host
ui-landing-feature-6-body  = One signed binary boots the operator console and the data plane. Toolchains needed for component builds (the Rust toolchain for Kanidm) auto-install into a sandboxed root and never touch your existing PATH state.
ui-landing-feature-7-title = Idempotent rollback
ui-landing-feature-7-body  = Every install path remembers what it changed -- service names, root directories, generated credentials -- and the rollback button can tear it down in exact reverse dependency order, leaving no orphan rows, no orphan units, no orphan secrets.
ui-landing-feature-8-title = Reseller-ready by design
ui-landing-feature-8-body  = Multi-tier license chains, white-label theming, channel-partner API contracts. Whether you sell direct, via a reseller, or via a sub-reseller, the platform encodes the relationship in the license and routes telemetry up the chain without leaking customer-private content.
ui-landing-feature-9-title = GUI-first, CLI-equivalent
ui-landing-feature-9-body  = Spec section 2.1 mandate: every administrative action reachable from the web console must also work from `computeza`. No "use the CLI" escape hatches, no operator-only flags, no production drift between your scripts and your operators' fingertips.

# --- Built for (target personas) ---

ui-landing-audiences-eyebrow  = Built for
ui-landing-audiences-title    = One control plane, three operator stories.
ui-landing-audiences-subtitle = Whether you are running this for yourself, your team, or a reseller chain, the surfaces below get out of your way.

ui-landing-audience-1-role  = Platform engineers
ui-landing-audience-1-title = Self-host the modern lakehouse without losing a quarter to YAML.
ui-landing-audience-1-body  = One binary boots the entire stack. The install wizard reads your hardware, picks safe defaults, and never asks you to write a manifest. Use the console for day-one bring-up, leave the CLI for CI; both drive the same code.
ui-landing-audience-2-role  = Enterprise operations
ui-landing-audience-2-title = On-prem compliance, audit, and observability without a sidecar zoo.
ui-landing-audience-2-body  = AES-256-GCM secrets, ed25519-signed audit, native OS services your ops team already monitors. SOC 2 and ISO controls map onto our surfaces line-by-line; the reseller chain claim on the license keeps procurement happy.
ui-landing-audience-3-role  = Resellers and OEMs
ui-landing-audience-3-title = Brand it, bill it, ship it as your data product.
ui-landing-audience-3-body  = The console renders through CSS-variable theming and accepts a tenant-supplied SVG mark. The license envelope carries your tier; the channel-partner API drives provisioning at scale; downstream telemetry aggregates upward without exposing customer content.

# --- Trust + compliance pillars (new section, between Features and Audiences) ---

ui-landing-trust-eyebrow  = Trust
ui-landing-trust-title    = Compliance evidence as a side-effect
ui-landing-trust-subtitle = Four built-in surfaces that shorten the regulated-buyer evaluation cycle. Every primitive ships in the binary; activation is operator-driven, never bolted on.

ui-landing-trust-1-title  = Signed license envelopes
ui-landing-trust-1-body   = Ed25519-signed multi-tier resale chain. Seat caps + expiry kill-switch enforced offline; the activation handler verifies against the binary's trusted root before persisting. Replace, deactivate, and audit every change.

ui-landing-trust-2-title  = Encrypted secrets at rest
ui-landing-trust-2-body   = AES-256-GCM under an Argon2id-derived key (m=64 MiB, t=3, p=1 -- OWASP 2025 baseline). Zeroize-on-drop. First-boot wizard generates and templates the systemd drop-in for you.

ui-landing-trust-3-title  = Post-quantum readiness
ui-landing-trust-3-body   = TLS handshakes already offer the hybrid X25519MLKEM768 group via rustls + aws-lc-rs. License envelopes carry a dual-signature shape (Ed25519 + ML-DSA / FIPS 204) so a future quantum break of Ed25519 alone does not invalidate entitlements.

ui-landing-trust-4-title  = EU AI Act deployer evidence
ui-landing-trust-4-body   = Annex IV model cards, Article 5 prohibited-classification refusal, Article 9-15 evidence checklist, Article 50 transparency primitive. Designed to support deployer compliance with Regulation (EU) 2024/1689 (effective for high-risk systems on 2 August 2026).

# --- Pricing ---

ui-landing-pricing-eyebrow  = Pricing
ui-landing-pricing-title    = Per-seat, paid-only. No free tier. No usage metering.
ui-landing-pricing-subtitle = Computeza is commercial software you install on your own hardware. We charge per operator seat; we never charge for compute, storage, query volume, or hosting -- because we never run any of those for you. Pricing scales linearly from small teams to multi-tier resale chains.

ui-landing-pricing-1-name     = Standard
ui-landing-pricing-1-price    = 49.99 EUR
ui-landing-pricing-1-unit     = / seat / month, up to 100 seats
ui-landing-pricing-1-tagline  = For SMB platform teams putting Computeza into production -- seat-capped, self-service, signed license.
ui-landing-pricing-1-feature-1 = Every one of the 11 managed components
ui-landing-pricing-1-feature-2 = Unified install + rollback + repair
ui-landing-pricing-1-feature-3 = Encrypted secrets store + rotate UI
ui-landing-pricing-1-feature-4 = Signed license envelope with seat cap + expiry kill-switch
ui-landing-pricing-1-feature-5 = Priority support (24h response)
ui-landing-pricing-1-feature-6 = Audit-log export to external SIEM
ui-landing-pricing-1-cta      = Talk to sales
ui-landing-pricing-1-badge    = Most popular

ui-landing-pricing-2-name     = Enterprise
ui-landing-pricing-2-price    = Custom
ui-landing-pricing-2-unit     = annual contract
ui-landing-pricing-2-tagline  = For regulated industries, sovereign workloads, and reseller chains. Personalised per-seat pricing negotiated on contract.
ui-landing-pricing-2-feature-1 = Everything in Standard, no seat cap
ui-landing-pricing-2-feature-2 = SLA-backed support (4h critical)
ui-landing-pricing-2-feature-3 = White-label theming + brand SVG
ui-landing-pricing-2-feature-4 = Multi-tier license chain + reseller billing
ui-landing-pricing-2-feature-5 = EU AI Act compliance + dedicated security review
ui-landing-pricing-2-feature-6 = Channel-partner gRPC API for provisioning
ui-landing-pricing-2-cta      = Contact sales

# --- Final CTA ---

ui-landing-final-title    = Run the lakehouse you already understand. On your hardware.
ui-landing-final-subtitle = Sign in to your console to lay down the data plane, or browse the eleven components Computeza manages. Either way, you stay in control.
ui-landing-final-primary  = Sign in
ui-landing-final-secondary = Browse components

# --- Login + setup + logout ---

ui-login-title             = Sign in
ui-login-intro             = Sign in with your operator account to drive the install / uninstall / rotate flows.
ui-login-username          = Username
ui-login-password          = Password
ui-login-submit            = Sign in
ui-login-failed            = Incorrect username or password. Try again, or restart the server and use the first-boot setup flow if you have lost the credentials.
ui-login-no-account        = No operator account yet?
ui-login-go-to-setup       = Run first-boot setup
ui-login-back-to-landing   = Back to the landing page

ui-setup-title             = First-boot setup
ui-setup-intro             = Create the first operator account for this Computeza install. After this account exists, the public /setup page is closed and additional operators are added from inside the console.
ui-setup-username          = Operator username
ui-setup-username-help     = ASCII alphanumeric, underscore, hyphen, or dot. 1 to 64 characters.
ui-setup-password          = Password
ui-setup-password-help     = At least 12 characters. We store the Argon2id hash, never the plaintext.
ui-setup-password-confirm  = Confirm password
ui-setup-submit            = Create account and sign in
ui-setup-already-done      = An operator account already exists on this Computeza install. The first-boot setup page is closed; sign in instead.
ui-setup-password-mismatch = The two password fields do not match. Re-type the password.

ui-nav-logout              = Sign out
ui-nav-signed-in-as        = Signed in as
ui-nav-account             = Account
ui-account-title           = Your account
ui-account-intro           = Details for your operator account. Signing out destroys the session cookie on this server and forces a sign-in on the next protected request.
ui-account-username        = Username
ui-account-session-since   = Session started

# --- Audit log viewer ---

ui-audit-title           = Audit log
ui-audit-intro           = Append-only event log for every administrative action on this Computeza install. Each entry is signed with the server's ed25519 audit key and chained to the previous entry via BLAKE3, so any tampering past the latest event is detectable. Newest events appear first; the viewer caps at 200 rows.
ui-audit-empty           = No audit events recorded yet. Sign in to the console or run any administrative action to populate the log.
ui-audit-missing         = No audit log is attached to this server. Re-run `computeza serve` with a writable state directory; the audit file lives next to the metadata store at `<state_db_parent>/audit.jsonl`.
ui-audit-col-seq         = #
ui-audit-col-timestamp   = Timestamp (UTC)
ui-audit-col-actor       = Actor
ui-audit-col-action      = Action
ui-audit-col-resource    = Resource
ui-audit-nav             = Audit
ui-audit-verifying-key   = Verifying key

# --- /admin/operators + /admin/groups ---

ui-admin-operators-title    = Operators
ui-admin-operators-intro    = Operator accounts that can sign in to this Computeza install. Each operator belongs to one or more groups; the union of group permissions determines what they can do in the console. Admins manage other operators here.
ui-admin-operators-col-username = Username
ui-admin-operators-col-groups   = Groups
ui-admin-operators-col-created  = Created
ui-admin-operators-col-actions  = Actions
ui-admin-operators-delete       = Delete
ui-admin-operators-delete-confirm = Permanently delete this operator? Their sessions are invalidated immediately; the action is irreversible.
ui-admin-operators-cant-delete-last-admin = Cannot delete the last admin: at least one admins-group operator must remain so the console retains a management surface.
ui-admin-operators-cant-delete-self = Cannot delete the account you are currently signed in as.
ui-admin-operators-edit-groups  = Update groups
ui-admin-operators-new-heading  = Create a new operator
ui-admin-operators-new-username = New operator username
ui-admin-operators-new-password = Initial password
ui-admin-operators-new-password-help = At least 12 characters. The operator can change it themselves once they sign in.
ui-admin-operators-new-groups   = Group memberships
ui-admin-operators-new-submit   = Create operator

ui-admin-groups-title  = Groups
ui-admin-groups-intro  = Built-in groups. v0.0.x ships three roles -- admins, operators, viewers -- with the permission sets shown below. Custom groups land in v0.1+.
ui-admin-groups-col-name = Group
ui-admin-groups-col-perms = Permissions

ui-nav-admin-operators = Operators
ui-nav-admin-groups    = Groups

# --- /admin/workspaces ---

ui-admin-workspaces-title = Workspaces
ui-admin-workspaces-intro = Workspaces isolate one tenant's resources from another's. v0.0.x ships a single `default` workspace that every existing install row falls under. Multi-tenant installs (v0.1+) will let the operator carve out per-tenant workspaces with separate quotas and reseller chains.
ui-admin-workspaces-col-name = Name
ui-admin-workspaces-col-created = Created
ui-admin-workspaces-default-note = `default` is the implicit workspace for every existing install row. Until v0.1 ships multi-tenant migration, you cannot create or rename workspaces from the console.

# --- /admin/branding ---

ui-admin-branding-title = Branding
ui-admin-branding-intro = White-label the operator console for resellers and OEMs. v0.0.x lets you override the accent color used across buttons, links, and gradient text; v0.1+ adds tenant-supplied SVG logos and footer support-routing text.
ui-admin-branding-accent = Accent color
ui-admin-branding-accent-help = Hex value, e.g. #C4B8E8 (the default lavender). Applied to every page via a CSS custom property override.
ui-admin-branding-submit = Save accent color
ui-admin-branding-saved = Branding updated. Reload the page to see the new accent color throughout the console.
ui-admin-branding-reset = Reset to default

# --- /admin/license ---

ui-admin-license-title = License
ui-admin-license-intro = Entitlement envelope for this Computeza install. v0.0.x renders the reseller chain + validity window read-only; v0.1+ adds the activation form and seat enforcement.
ui-admin-license-none = No license has been activated on this install. Computeza is paid-only commercial software. Contact your reseller, or hello@computeza.eu for direct deals, to obtain a signed license envelope; until one is activated, install paths still work but features that depend on entitlements (seat caps, tier gates, expiry kill-switch, channel-partner API) remain dormant.
ui-admin-license-tier = Tier
ui-admin-license-seats = Seat cap
ui-admin-license-chain = Resale chain
ui-admin-license-not-before = Valid from
ui-admin-license-not-after = Valid until

ui-nav-admin-workspaces = Workspaces
ui-nav-admin-branding   = Branding
ui-nav-admin-license    = License

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
ui-nav-workspace      = Workspace
ui-nav-install        = Install

# --- Workspace (catalog browser + SQL editor) ---
ui-workspace-title              = Workspace
ui-workspace-intro              = Browse the Iceberg catalog (Lakekeeper) and run SQL against the lakehouse engine (Databend). Phase 1 v0.0.x scope: catalog read + SQL execution against the local installation. Drag-and-drop pipelines, notebooks, and dedicated compute groups land in subsequent phases.
ui-workspace-catalog-heading    = Catalog
ui-workspace-catalog-empty      = No namespaces found. Either the Lakekeeper instance is unreachable or the catalog is empty -- try `CREATE NAMESPACE` from the SQL editor on the right, or check /status for the Lakekeeper reconciler.
ui-workspace-catalog-fill-link  = Pre-fill SELECT *
ui-workspace-sql-heading        = SQL
ui-workspace-sql-placeholder    = SELECT 1
ui-workspace-sql-run            = Run query
ui-workspace-sql-help           = Hits Databend's HTTP query handler. Reads + writes from this editor are unrestricted in v0.0.x; per-user RLS lands in the Functions milestone (see AGENTS.md "Deferred work").
ui-workspace-results-heading    = Results
ui-workspace-results-empty      = Run a query above to see results.
ui-workspace-error-no-lakekeeper = No Lakekeeper instance is registered. Install Lakekeeper from /install before using the catalog browser.
ui-workspace-error-no-databend   = No Databend instance is registered. Install Databend from /install before running SQL.
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
ui-install-card-configured     = Reviewed
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

# --- Generated credentials on the result page (one-time view) ---

ui-install-credentials-title   = Generated credentials
ui-install-credentials-warning = Copy these values out of this page now. They will not be shown again on a refresh. When the secrets store is attached they remain recoverable through the rotate-secrets UI; without the secrets store this is the only place they appear.
ui-install-credentials-component = Component
ui-install-credentials-username  = Username
ui-install-credentials-password  = Password
ui-install-credentials-ref       = Secrets ref

# --- Rollback button on the install result page ---

ui-install-rollback-title    = Roll back this install
ui-install-rollback-intro    = Uninstall every component this job successfully laid down, in reverse dependency order. Use this when a later component failed and you want a clean re-try, or when you want to fully tear down a successful install. Best-effort: failures during teardown are logged but do not stop the rollback.
ui-install-rollback-button   = Roll back this install

# --- Repair / re-install on the resource detail page ---

ui-resource-repair-heading = Repair this component
ui-resource-repair-intro   = Re-run the install for this component. Idempotent: existing on-disk state is preserved where possible and only out-of-date pieces are regenerated. Use this when the service is down, the systemd unit went missing, or the spec was edited externally and you want the install path to re-apply it.
ui-resource-repair-button  = Re-install

# --- Secrets admin page ---

ui-secrets-title         = Secrets
ui-secrets-intro         = Encrypted secrets stored under the AES-256-GCM data-encryption key derived from `COMPUTEZA_SECRETS_PASSPHRASE`. Each entry's value never touches disk in plaintext; names are stored in clear so this page can list them.
ui-secrets-backup-warning = Disaster-recovery reminder: to recover these secrets on another host you MUST back up THREE things together -- the COMPUTEZA_SECRETS_PASSPHRASE value, the salt file (computeza-secrets.salt next to your metadata store), and the encrypted ciphertext file (computeza-secrets.jsonl in the same directory). Lose any one of these and every value here becomes permanently unrecoverable. There is no master recovery path by design.
ui-secrets-empty         = The secrets store is attached but no secrets are stored yet. New entries land here as soon as an install path generates them (e.g. initial admin passwords).
ui-secrets-store-missing = No secrets store is attached on this server. Set `COMPUTEZA_SECRETS_PASSPHRASE` in the environment and restart `computeza serve` to enable encrypted secret storage; until then install paths surface generated credentials in-band on the result page only.
ui-secrets-col-name      = Name
ui-secrets-col-action    = Action
ui-secrets-rotate-button = Rotate
ui-secrets-rotate-note   = Rotating replaces the value with a fresh 96-bit random hex string. The previous value is unrecoverable; downstream consumers must be updated to the new value before they next authenticate. The new value is shown once on the result page.
ui-secrets-rotated-title = Secret rotated
ui-secrets-rotated-name  = Secret name
ui-secrets-rotated-value = New value
ui-secrets-rotated-back  = Back to secrets
ui-nav-admin             = Admin
ui-admin-secrets         = Secrets

# --- Active-job resume banner on /install ---

ui-install-active-title   = Install in progress
ui-install-active-resume  = Resume the wizard
ui-install-active-running = currently running

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
ui-install-status-installed  = Installed

# Per-card singular action labels: when a component is already
# installed, the operator can tear down just that one; when it isn't,
# the operator can install just that one without running the bulk
# Install-All flow.
ui-install-card-action-install      = Install just this one
ui-install-card-action-uninstall    = Uninstall just this one
ui-install-card-action-help-installed = This component is installed. Use this if you want to tear down just this one (e.g. to re-install with different settings) without affecting the rest of the cluster.
ui-install-card-action-help-missing   = Install this single component without running the bulk Install-All flow. Useful when one earlier install failed and you want to retry just that component.
ui-install-coming-soon-title = Install from the CLI
ui-install-coming-soon-body  = This component's native install path is wired in `computeza-driver-native` for Ubuntu Linux (download + systemd unit + start). The per-component web wizard lands in a follow-up commit. Today: run `computeza install <slug>` from the CLI on an Ubuntu host (22.04 LTS or 24.04 LTS). macOS + Windows + non-Ubuntu Linux driver variants are v0.1+; the reconciler is already wired to observe any running instance once its spec is in the metadata store.
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
ui-install-data-dir-help     = Where the cluster files live. Leave blank for the default at /var/lib/computeza/postgres (Ubuntu Linux). v0.0.x is Ubuntu-only; macOS + Windows defaults will return when their drivers land in v0.1+.
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
ui-uninstall-intro    = This rolls back the install: stops the systemd unit, deletes the data directory, removes the psql shim from PATH, and drops postgres-instance/local from the metadata store. The cached binary bundle is preserved so re-install is fast.
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
