# Milestone plan -- Pattern A SSO (Kanidm as the single sign-on hub)

**Status:** plan, not implementation. Drafted 2026-05-13.

**Scope:** one Kanidm tenant becomes the source of truth for every operator and end-user across the 11 managed components. No external IdP. One username/password gets you into the operator console, Grafana, OpenFGA, Lakekeeper, Restate, and Postgres (via JWT). External IdP federation (Entra ID / Okta / Auth0 / Google / Keycloak) is Pattern B, deferred -- the foundation crate `computeza-identity-federation` (E19) already exists and Pattern A's plumbing is forward-compatible with it.

**Why this first:** Pattern A is the highest-value, lowest-cost SSO milestone we can ship. It removes the "11 different admin credentials per install" problem operators complain about today, slots cleanly into the existing kanidm install path, and leaves Pattern B as a Kanidm-side configuration change rather than another Computeza-code change.

---

## Outcome shape

After this milestone lands, a fresh Computeza install behaves as follows on first boot:

1. Operator runs the install wizard, picks "Enable SSO" (default on; off requires explicit opt-out).
2. Kanidm installs first (it's the IdP -- everything else depends on it).
3. Each subsequent component install registers an OAuth/OIDC client against the running Kanidm, writes its component-specific config pointing at Kanidm's issuer URL, and starts up.
4. The Computeza operator console (computeza serve) federates `/login` against Kanidm.
5. The operator creates their **first admin in Kanidm** via the existing `kanidmd recover_account` flow; that account then signs into the console.
6. Every subsequent component admin URL (Grafana, OpenFGA, Lakekeeper, Restate, Databend, ...) accepts the same Kanidm-issued session via OIDC.
7. Postgres uses JWT auth where the operator's Kanidm token signs DB connection attempts (via the `pgjwt` extension or equivalent); fallback to per-component password for clients that can't do OIDC.

---

## Component-by-component federation strategy

Each managed component has a different SSO posture. The table sets expectations per component.

| Component | Protocol | Computeza side wiring | Difficulty |
|-----------|---|---|---|
| Kanidm | -- (hub) | Install + initial admin recovery already wired | trivial |
| Operator console | OIDC code+PKCE | Already half-built via E19; add callback handler + session minting | low |
| Grafana | Generic OAuth | Patch `grafana.ini` with `[auth.generic_oauth]` block referencing Kanidm | low |
| OpenFGA | OIDC bearer tokens | Configure `OPENFGA_AUTHN_METHOD=preshared` -> `oidc`; supply Kanidm JWKS URL | medium |
| Lakekeeper | OIDC | Lakekeeper supports OIDC since v0.10; supply issuer URL + client_id | medium |
| Restate | OIDC for admin API | Restate's admin API supports JWT bearer; configure issuer + audience | medium |
| Databend | LDAP or OIDC (HTTP handler) | Configure HTTP handler to validate Kanidm JWTs | medium |
| Postgres | JWT via `pg_jwt_verify` extension | Install extension, patch `pg_hba.conf` for JWT auth method, supply Kanidm JWKS | high |
| Qdrant | API keys (no OIDC support today) | Out of scope for Pattern A; document as a per-component admin token | -- |
| GreptimeDB | API keys (no OIDC) | Same -- document, don't gate | -- |
| Apache XTable | -- (batch job, no auth surface) | n/a | -- |

Trade-off: Qdrant + GreptimeDB don't speak OIDC today. We surface their admin tokens via the existing one-shot credentials view; Pattern A doesn't pretend to cover them. The "single sign-on" promise applies to the 8 components that **have** an auth surface that takes JWTs.

---

## Implementation phases

### Phase 1 -- Console-side OIDC callback (1-2 days)

Extend the existing `computeza-identity-federation` crate (E19) so the console can actually complete an OIDC flow rather than just begin one.

**Files / surfaces:**
- `crates/computeza-identity-federation/src/providers.rs` -- add `exchange_code_for_token` (currently `NotImplemented`).
- `crates/computeza-ui-server/src/lib.rs` -- add routes:
  - `GET /auth/oidc/:provider/start` -- builds authorization URL, sets `oidc_state` + `oidc_pkce` cookies, redirects to the IdP.
  - `GET /auth/oidc/:provider/callback?code=...&state=...` -- exchanges code, validates ID-token signature against the IdP's JWKS, mints a Computeza session.
  - `/login` page gains a "Sign in with Kanidm" button alongside the existing password form.
- `crates/computeza-ui-server/src/auth.rs` -- session minting accepts an `OidcSession` shape carrying `sub`, `email`, `groups` (the `groups` claim drives Computeza RBAC group mapping).

**Tests:**
- Unit: `exchange_code_for_token` happy path against a `wiremock`-served fake IdP (reuses the harness from E19).
- Unit: callback handler rejects mismatched state, expired nonce, missing PKCE verifier.
- Integration: full code -> session mint against a Kanidm dev instance booted in CI (Kanidm has a `kanidm_unixd` Docker image we can pin).

### Phase 2 -- Kanidm OAuth-client provisioning helper (2-3 days)

The shared shape: every component-install handler can call `kanidm_register_oauth_client(slug, redirect_uri, scopes)` which:

1. Talks to the running Kanidm's admin API.
2. Registers a confidential OAuth2 client named `computeza-<slug>` with the supplied redirect URI.
3. Returns the generated `client_id` + `client_secret` for the caller to embed in the component config.
4. Stores both in the encrypted secrets store under `<slug>/oidc-client-id` and `<slug>/oidc-client-secret`.

**Files:**
- New crate or module: `crates/computeza-identity-federation/src/kanidm_client.rs`.
- Uses the existing `reqwest` client; mTLS against Kanidm's loopback admin port.

**Tests:**
- Unit against `wiremock` mimicking Kanidm's `/v1/oauth2` API surface.
- Integration: provision a real Kanidm container, register a client, verify the client is enumerated via `kanidm-cli oauth2 list`.

### Phase 3 -- Per-component config templates (3-5 days, parallelisable)

For each of the 8 OIDC-capable components, add a config-template function that injects the Kanidm issuer URL + client_id/secret. Each component's existing driver gains a feature-flagged code path:

- `grafana.rs::install_with_sso` writes `grafana.ini`'s `[auth.generic_oauth]` block.
- `openfga.rs::install_with_sso` sets `OPENFGA_AUTHN_METHOD=oidc` env vars in the systemd unit.
- `lakekeeper.rs::install_with_sso` adds the `LAKEKEEPER__AUTHENTICATION__OIDC` config keys.
- `restate.rs::install_with_sso` patches the admin API config with `auth.jwt.issuer`.
- `databend.rs::install_with_sso` writes the HTTP handler's `jwt_key_files` referring to Kanidm's JWKS.
- `postgres.rs::install_with_sso` -- larger: install `pg_jwt_verify` (currently in `pgxn`; v0.1 of the driver can bundle a build), patch `pg_hba.conf` with a `host all all 0.0.0.0/0 jwt issuer=https://kanidm.local`.
- `kanidm.rs` -- already the hub; no client config needed.
- Console (computeza-ui-server) -- consumes the OIDC client via Phase 1.

Each component test gets one new variant: install with SSO enabled, verify the config file contains the issuer URL + client_id and no plaintext password.

### Phase 4 -- Install-wizard UX (1-2 days)

Add a top-level "Enable SSO" checkbox on `/install` that:
- Defaults **on**.
- Greys out per-component "admin password" fields when checked (no plaintext admin needed; admins come from Kanidm).
- After install completes, the result page shows ONE credential: the Kanidm admin recovery instruction. Not 11 separate passwords.

The credentials JSON download (`b34bbd0`) becomes a much shorter file in SSO mode -- typically just Qdrant + GreptimeDB API tokens for the two components that don't speak OIDC.

### Phase 5 -- Audit + ops surfaces (1 day)

- Sign-in events from the OIDC callback emit `Action::Authn` audit entries with the IdP-issued `sub` claim, matching what password sign-in does today.
- `/admin/operators` learns to render an "Identity source" column distinguishing local vs OIDC.
- `/admin/operators` create-form is disabled in SSO mode (operators come from Kanidm); the existing seat-cap enforcement still applies.

---

## Out of scope for this milestone

- **Pattern B external IdP federation.** The console gains the OIDC callback in Phase 1, which is the same machinery Pattern B will reuse; the difference is which IdP it points at. Pattern B is a follow-up that wires a Kanidm-side OIDC client trusting Entra ID / Okta / etc., not a Computeza-code change.
- **SCIM provisioning.** When external IdPs are wired (Pattern B), some buyers want users automatically synced into Kanidm rather than manually created. Defer; non-blocking for the SSO experience itself.
- **Multi-tenant Kanidm.** v0.0.x runs a single Kanidm tenant per install. Multi-tenant tenancy at the IdP layer is v1.0+.
- **Passkeys + WebAuthn.** Kanidm supports them out of the box. We don't need to wire anything; operators enable them in Kanidm's admin UI and they "just work" against every OIDC-fronted surface.

---

## Risks + mitigations

| Risk | Mitigation |
|---|---|
| Kanidm's admin API auth model is mTLS-only -- needs the Computeza serve process to hold a Kanidm-issued client cert. | Phase 2's `kanidm_register_oauth_client` runs during the kanidm install step, where we still have the bootstrap admin password from `kanidmd recover_account`; the client cert is generated at that point and stored in the encrypted secrets store. |
| Postgres JWT auth (`pg_jwt_verify`) is a non-trivial extension build. | Acceptable to ship Pattern A without Postgres JWT in v0.1; document the workaround (per-DB role + Kanidm-issued password). Promote to JWT in v0.2 once we have the extension build pipeline. |
| Operators reset Kanidm's admin password independently and lock out the Computeza console. | The console renders a `/admin/identity/recover` page that walks them through `kanidmd recover_account admin` on the host and re-anchoring the console's OAuth client. Same flow as today's "wrong password" recovery, just for the SSO surface. |
| OIDC clock skew between Kanidm and the components causes random JWT-validation failures. | Standard `leeway` of 60s in the JWT verifier (every component supports this knob); document in the install guide. |

---

## Total effort estimate

- Phase 1: 1-2 days
- Phase 2: 2-3 days
- Phase 3: 3-5 days (parallelisable across components)
- Phase 4: 1-2 days
- Phase 5: 1 day

**Realistic: 1.5-2 calendar weeks** for one engineer working focused, with Pattern B as a separate follow-up of similar size.
