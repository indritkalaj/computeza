# Security audit policy

Computeza ships software that customers install on infrastructure they
own. The CVE posture of the dependency tree is therefore part of the
product, not a back-office concern. This document codifies how we
audit, when we accept advisories, and how the policy is enforced in CI.

## What runs

The `cargo audit` job in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)
fetches the [RustSec advisory database](https://rustsec.org/) on every
push and pull request and scans `Cargo.lock` against it. Three classes
of finding are surfaced:

| Class | Example | Default policy |
|---|---|---|
| **Vulnerability** (`error: N vulnerabilities found`) | A CVE with severity, e.g. RUSTSEC-2023-0071 | Blocks the job. Must be patched, ignored with a written rationale, or escalated. |
| **Warning: unmaintained** | RUSTSEC-2024-0436 `paste` | Surfaced. Tolerated transitively only when no reachable harm and no upstream replacement. |
| **Warning: unsound** | RUSTSEC-2025-0067 `libyml` | Surfaced. Same posture as unmaintained but bar for tolerance is higher. |

The job runs with two `--ignore` flags; everything else is
release-blocking. We pin both the flag list and the rationale inline in
the workflow so the trail is auditable and never goes stale silently.

## Current ignores

As of `e460fbf` (2026-05-11):

### `RUSTSEC-2023-0071` -- `rsa` Marvin Attack

Severity: medium (5.9). "Potential key recovery through timing
sidechannels." No upstream fix is published.

Pulled in transitively by `sqlx-mysql`, which itself is dragged in by
`sqlx-macros` because the macro crate depends on every backend's row
types regardless of which backends the consumer enables via features.
Our `sqlx` feature set is `postgres + sqlite` -- we never instantiate a
MySQL connection at runtime, so the timing channel is unreachable.

Re-evaluate when (a) the `rsa` crate ships a fix, or (b) sqlx changes
its macros to optional-feature the backend deps.

### `RUSTSEC-2024-0436` -- `paste` unmaintained

The `paste` crate is no longer maintained but still functional. It's
pulled in by every Leptos sub-crate (`tachys`, `reactive_graph`,
`reactive_stores`, `leptos_dom`, `leptos_server`, `leptos`, `either_of`,
...). Removing this dependency requires Leptos itself to migrate, which
we can't force.

Re-evaluate when Leptos ships a release that drops `paste`.

## What we did *not* ignore

The 2025-09-11 advisories against `serde_yml` (RUSTSEC-2025-0068
unsound/unmaintained) and `libyml` (RUSTSEC-2025-0067 unsound) were
**not** ignored. Instead we dropped the dependency outright. The only
consumer was `computeza-pipelines`, which is still a stub crate with no
real YAML loading. When pipeline YAML support lands we will evaluate
`serde-norway`, `serde-yaml-bw`, and `marked-yaml` against current
advisories and choose a maintained option.

This is the preferred fix when the dep is removable. Ignoring a
known-bad crate just because the surface is small invites later
surprise when somebody else in the codebase reaches for it.

## Adding or removing an ignore

1. Run `cargo audit` locally and read the offending advisory in full.
2. Decide: patch, replace, drop the dep, or ignore.
3. **Ignore requires written rationale.** Document in this file *and* in
   the workflow comment. The rationale must say why the advisory is
   unreachable in our usage, why no fix path exists upstream, and what
   trigger should cause us to re-evaluate.
4. Add `--ignore RUSTSEC-YYYY-NNNN` to the workflow's `cargo audit`
   invocation.
5. Open a tracking issue tagged `security: audit-ignore` so the
   re-evaluation isn't lost. (Issue templates land when GitHub Issues
   is set up; until then a TODO comment with the date is enough.)

## Releasing without unaccepted advisories

The `cargo audit` job is a release-blocker. A release branch is
considered ready only when:

- `cargo audit` exits zero (with the documented `--ignore` flags).
- No new advisory has been published against a crate in the dep tree
  since the last release evaluation. Re-run `cargo audit` immediately
  before cutting the release tag.
- Every active `--ignore` has been re-evaluated against the current
  advisory text -- not just the ID -- in case upstream amended scope
  or severity.

The first time an `--ignore` line outlives its rationale is the first
time we should have removed it. Stale ignores are a security
anti-pattern; treat them as such.

## See also

- [`AGENTS.md`](../AGENTS.md) hard rule 3 (latest stable deps).
- [`docs/contributing.md`](./contributing.md) for the local dev loop.
- Spec section 8 (security & threat model) for the broader posture this
  fits inside.
