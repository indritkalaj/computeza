# Contributing to Computeza

This document captures the bits of the contributor experience that
aren't already in [`README.md`](../README.md) or [`CLAUDE.md`](../CLAUDE.md).

## Local development loop

```sh
./scripts/check.sh            # full gate: fmt + ascii + check + clippy + test
./scripts/check.sh --quick    # fast: fmt + ascii + check only
```

If the full gate is green here, CI on GitHub will be green too -- the
script mirrors the workflow at `.github/workflows/ci.yml` exactly.

When the gate fails, the script prints which step caught the issue and
how to fix it. Common failures:

- **rustfmt diffs** -- run `cargo fmt --all` to apply.
- **ASCII guard fails** -- run `python3 scripts/audit-nonascii.py` to
  see the non-ASCII characters present. Most can be auto-fixed with
  `python3 scripts/audit-fix-nonascii.py`; anything unusual needs a
  manual decision (em-dash to `--`, section sign to `section `, etc.).
  See CLAUDE.md hard rule 6.
- **clippy warnings** -- the workspace runs with `clippy::all = "warn"`
  and `RUSTFLAGS=-D warnings`, so any clippy lint fails CI. Apply the
  fix clippy suggests, or document an `#[allow]` with a comment if the
  lint is genuinely wrong.

## House rules

The non-negotiable rules live in [`CLAUDE.md`](../CLAUDE.md). They are:

1. No hardcoded user-facing strings -- everything through `computeza-i18n`.
2. GUI-first -- every admin operation reachable from the web console.
3. Latest stable for every dep -- no deprecated, no abandoned.
4. Single binary, autonomous installer -- and PATH-registration on each OS.
5. The spec wins.
6. ASCII-only source.
7. Detailed actionable logs at every level.

## Commit style

Subject line starts with the affected crate or area prefix:

```
reconciler-postgres: emit signed audit events on every change
ui-server: serve Tailwind-compatible CSS asset
ci: drop --locked until Cargo.lock is committed
audit: append-only Ed25519-signed log with hash chaining
docs: add contributing guide
```

The body explains *why*, not just *what*. Co-author trailers welcome.

## Audit scripts

[`scripts/audit-nonascii.py`](../scripts/audit-nonascii.py) scans every
tracked source-shaped file for bytes outside US-ASCII and prints a
per-file summary with hex codepoints.

[`scripts/audit-fix-nonascii.py`](../scripts/audit-fix-nonascii.py)
applies the canonical substitution table (em-dash to `--`, arrows to
`->` / `<-`, section sign to `section `, etc.). Re-run the scanner
afterwards to confirm zero remains; commit the result.

The same regression net runs in CI as the `ascii-only` job, so the
substitutions are enforced for every PR, not just trusted locally.

## Where things live

| Layer | Crate(s) | Spec ref |
|---|---|---|
| Foundation traits | `computeza-core` | sections 3.3-3.5 |
| Persistence | `computeza-state` (SQLite) | section 3.1 |
| Audit log | `computeza-audit` (Ed25519 + chain) | sections 3.5, 4.5 |
| Secrets | `computeza-secrets` (AES-GCM + Argon2id) | section 3.2 |
| OS-level deploy | `computeza-driver-native` (Linux only at v0.0.x) | section 10 |
| Component reconcilers | `computeza-reconciler-*` | section 7 |
| Web UI | `computeza-ui-*` (Leptos SSR + Tailwind utilities) | section 4 |
| i18n | `computeza-i18n` (Fluent `.ftl`) | section 4.1 |
| Binary | `computeza` (CLI + serve + install) | sections 2.1, 10 |

Pre-alpha: most reconciler crates are read-only observe; write paths
land as the platform reconcile loop matures.
