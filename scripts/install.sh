#!/usr/bin/env bash
#
# Install (or uninstall) the favetto daemon as a systemd *user* service.
#
# favetto keeps its data and config under the calling user's home
# (`~/.local/share/favetto`, `~/.config/favetto`) and launches per-user agent
# CLIs, so it is installed as a `systemctl --user` unit. This script never uses
# `sudo`.
#
# Usage:
#   scripts/install.sh                 # install + enable + start
#   scripts/install.sh --no-start      # install but do not enable/start
#   scripts/install.sh --uninstall     # stop, disable and remove the unit
#   scripts/install.sh --uninstall --purge
#                                      # ... and delete config + data too
#
# Environment overrides honoured: XDG_CONFIG_HOME, XDG_DATA_HOME, CARGO_HOME.
set -euo pipefail

SERVICE_NAME="favetto.service"

# Resolve the repository root from this script's location so it works from any
# working directory.
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
unit_dir="$config_home/systemd/user"
unit_src="$repo_root/packaging/systemd/favetto.service"
unit_dst="$unit_dir/$SERVICE_NAME"
cfg_dir="$config_home/favetto"
cfg_file="$cfg_dir/config.toml"
data_dir="$data_home/favetto"

usage() {
    cat <<'EOF'
Install the favetto daemon as a systemd user service.

Usage: scripts/install.sh [OPTIONS]

Options:
  --uninstall   Stop, disable and remove the unit (keeps user data).
  --purge       With --uninstall, also delete the config and data directories.
  --no-start    Install the unit but do not run `systemctl --user enable --now`.
  -h, --help    Show this help and exit.

The script builds and installs the `favetto` binary with cargo, seeds
~/.config/favetto/config.toml from config.example.toml when missing, installs
the unit from packaging/systemd/favetto.service, and reloads the user manager.
EOF
}

log() { printf 'favetto: %s\n' "$*"; }
warn() { printf 'favetto: warning: %s\n' "$*" >&2; }
die() {
    printf 'favetto: error: %s\n' "$*" >&2
    exit 1
}

uninstall=0
purge=0
no_start=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --uninstall) uninstall=1 ;;
        --purge) purge=1 ;;
        --no-start) no_start=1 ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            usage >&2
            die "unknown option: $1"
            ;;
    esac
    shift
done

if [ "$purge" -eq 1 ] && [ "$uninstall" -eq 0 ]; then
    die "--purge is only valid together with --uninstall"
fi

# True when `systemctl --user` is usable (a user manager is running).
user_manager_available() {
    systemctl --user daemon-reload >/dev/null 2>&1
}

# The resolved path of the installed binary; prefer the cargo home, then PATH,
# then the conventional default. Used to substitute `ExecStart` in the unit.
resolve_bin_path() {
    if [ -n "${CARGO_HOME:-}" ] && [ -x "$CARGO_HOME/bin/favetto" ]; then
        printf '%s\n' "$CARGO_HOME/bin/favetto"
    elif command -v favetto >/dev/null 2>&1; then
        command -v favetto
    else
        printf '%s\n' "$HOME/.cargo/bin/favetto"
    fi
}

# Warn (but do not fail) when no supported agent CLI is on PATH: favetto cannot
# run a task without one.
warn_if_no_agent() {
    for agent in opencode claude pi vibe; do
        if command -v "$agent" >/dev/null 2>&1; then
            return 0
        fi
    done
    warn "no supported agent CLI (opencode, claude, pi, vibe) found on PATH;"
    warn "tasks will fail to launch until one is installed and reachable."
}

uninstall_service() {
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user disable --now "$SERVICE_NAME" >/dev/null 2>&1 || true
    fi

    if [ -f "$unit_dst" ]; then
        rm -f "$unit_dst"
        log "removed $unit_dst"
    else
        log "no unit installed at $unit_dst"
    fi

    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload >/dev/null 2>&1 || true
    fi

    if [ "$purge" -eq 1 ]; then
        rm -rf "$cfg_dir" "$data_dir"
        log "purged $cfg_dir and $data_dir"
    else
        log "kept user data in $cfg_dir and $data_dir (pass --purge to delete)"
    fi

    log "uninstalled $SERVICE_NAME"
}

install_service() {
    [ -f "$unit_src" ] || die "unit template not found: $unit_src"

    if ! command -v cargo >/dev/null 2>&1; then
        die "cargo not found on PATH; install Rust from https://rustup.rs first"
    fi

    log "installing the favetto binary (cargo install --locked)"
    cargo install --path "$repo_root/crates/favetto" --locked --force

    bin_path="$(resolve_bin_path)"
    [ -x "$bin_path" ] || die "installed binary not found at $bin_path"
    log "using binary $bin_path"

    mkdir -p "$cfg_dir"
    if [ ! -f "$cfg_file" ]; then
        cp "$repo_root/config.example.toml" "$cfg_file"
        log "seeded $cfg_file from config.example.toml"
    else
        log "keeping existing $cfg_file"
    fi

    mkdir -p "$unit_dir"
    # Substitute the resolved binary path into the checked-in unit's ExecStart.
    sed "s|%h/.cargo/bin/favetto|$bin_path|" "$unit_src" >"$unit_dst"
    log "installed $unit_dst"

    warn_if_no_agent

    if ! user_manager_available; then
        warn "systemctl --user is not available (no running user manager)."
        warn "The binary, config and unit are in place; log in again or run"
        warn "'systemctl --user daemon-reload && systemctl --user enable --now $SERVICE_NAME'."
        return 1
    fi

    if [ "$no_start" -eq 1 ]; then
        log "unit installed (--no-start); enable it with: systemctl --user enable --now $SERVICE_NAME"
    else
        systemctl --user enable --now "$SERVICE_NAME"
        log "enabled and started $SERVICE_NAME"
    fi

    cat <<EOF

favetto is installed as a systemd user service.

  status:  systemctl --user status $SERVICE_NAME
  logs:    journalctl --user -u $SERVICE_NAME -f
  restart: systemctl --user restart $SERVICE_NAME
  stop:    systemctl --user stop $SERVICE_NAME
  attach:  favetto tui

Agent CLIs must be reachable from the unit's PATH; edit the Environment=PATH
line in $unit_dst if they live elsewhere.
EOF
}

if [ "$uninstall" -eq 1 ]; then
    uninstall_service
else
    install_service
fi
