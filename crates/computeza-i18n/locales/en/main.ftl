# Computeza -- English (en) message bundle.
#
# This file holds user-facing strings for the CLI surface and any
# cross-cutting messages that don't belong to a specific subsystem.
# Subsystem-specific bundles live in their own .ftl files (e.g.
# `cli.ftl`, `errors.ftl`, `installer.ftl`) and are loaded together
# by the static_loader! call in src/lib.rs.
#
# When adding a new message:
#   - keys are kebab-case
#   - prefer message references and arguments over string concatenation
#   - keep message text short; long-form copy belongs in markdown docs

# --- Top-level CLI banner ---

welcome-banner = Computeza -- open lakehouse control plane
welcome-help   = Run `computeza --help` to see available commands.

# --- Subcommand placeholders (will be replaced as commands are implemented) ---

cmd-install-todo = `computeza install <component>` -- installs a managed component natively. v0.0.x supports `postgres` on Linux only.
cmd-serve-todo   = `computeza serve` -- start the operator console (web UI + reconciler). Not yet implemented in this scaffold.
cmd-status-todo  = `computeza status` -- report cluster health and reconciliation drift. Not yet implemented in this scaffold.
cmd-license-todo = `computeza license` -- show license tier, seat usage, activation health, expiry. Not yet implemented in this scaffold.

# --- Install errors / platform gates ---

install-postgres-linux-only = `computeza install postgres` is only supported on Linux in v0.0.x. macOS (launchd) and Windows (Services) variants ship in a follow-up release.
install-postgres-unsupported-os = `computeza install postgres` only supports Linux (systemd) and macOS (launchd) in v0.0.x. Windows (Services) ships in a follow-up release.

# --- Status command ---

status-healthy     = OK operator console is healthy
status-unhealthy   = FAIL operator console returned an error
status-unreachable = FAIL operator console is unreachable

# --- Generic ---

err-unknown    = An unexpected error occurred. See logs for details.
err-not-impl   = This action is not yet implemented.
