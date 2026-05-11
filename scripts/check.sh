#!/usr/bin/env bash
# scripts/check.sh -- run the full local CI gate.
#
# Mirrors the steps the GitHub Actions workflow runs so an `OK` here
# means CI will be green. Exits non-zero on the first failure with a
# pointer to the offending tool.
#
# Usage:
#     ./scripts/check.sh           # full gate
#     ./scripts/check.sh --quick   # skip clippy + test (fmt + check only)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

quick=false
if [[ "${1:-}" == "--quick" ]]; then
    quick=true
fi

step() {
    printf '\n>>> %s\n' "$*" >&2
}

step "cargo fmt --all --check"
if ! cargo fmt --all --check; then
    echo
    echo "rustfmt found diffs. Run \`cargo fmt --all\` to apply." >&2
    exit 1
fi

step "ascii-only guard (no non-ASCII bytes in tracked source)"
# Prefer python3 on Linux / macOS; fall back to python on Windows where
# `python3` is often a Microsoft-Store shim that exits non-zero. We test
# `python3 --version` rather than just `command -v` because the shim
# satisfies `command -v` but fails on actual invocation.
PY=""
if python3 --version >/dev/null 2>&1; then
    PY=python3
elif python --version >/dev/null 2>&1; then
    PY=python
else
    echo "Python is required for the ascii audit step; install python3 (Linux/macOS) or python (Windows)" >&2
    exit 1
fi
"$PY" scripts/audit-nonascii.py | tail -1 | grep -q '^TOTAL files-with-nonascii: 0$' || {
    echo "Found non-ASCII bytes. Run $PY scripts/audit-nonascii.py" >&2
    echo "to see offenders, then $PY scripts/audit-fix-nonascii.py" >&2
    exit 1
}

step "cargo check --workspace --all-targets"
cargo check --workspace --all-targets

if $quick; then
    echo
    echo "OK (quick gate)"
    exit 0
fi

step "cargo clippy --workspace --all-targets"
cargo clippy --workspace --all-targets

step "cargo test --workspace"
cargo test --workspace

echo
echo "OK (full gate)"
