#!/usr/bin/env bash
#
# Install (or uninstall) the favetto daemon as a systemd *user* service.
#
# favetto keeps its data and config under the calling user's home
# (`~/.local/share/favetto`, `~/.config/favetto`) and launches per-user agent
# CLIs, so it is installed as a `systemctl --user` unit. This script never uses
# `sudo`.
#
# Secrets for scheduled/action tasks (e.g. the `GITHUB_TOKEN` a poll gate
# interpolates) are read from a private `~/.config/favetto/env` file that the
# unit loads via `EnvironmentFile=`. The script creates it (mode 600) when
# missing and seeds `GITHUB_TOKEN` from an authenticated `gh` unless
# `--no-github-token` is passed; it never overwrites an existing value.
#
# Usage:
#   scripts/install.sh                 # build the web UI, install + enable + start
#   scripts/install.sh --no-web        # skip building the embedded web client
#   scripts/install.sh --no-start      # install but do not enable/start
#   scripts/install.sh --uninstall     # stop, disable and remove the unit
#   scripts/install.sh --uninstall --purge
#                                      # ... and delete config + data too
#
# The Dioxus web client is built into `web/dist` first, because the daemon embeds
# that directory with `rust-embed` at compile time. It therefore needs the
# `dioxus-cli` (`dx`) and the `wasm32-unknown-unknown` target; pass `--no-web` to
# install the daemon without the embedded UI (it then serves a placeholder page).
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
env_file="$cfg_dir/env"
data_dir="$data_home/favetto"

usage() {
    cat <<'EOF'
Install the favetto daemon as a systemd user service.

Usage: scripts/install.sh [OPTIONS]

Options:
  --uninstall   Stop, disable and remove the unit (keeps user data).
  --purge       With --uninstall, also delete the config and data directories.
  --no-start    Install the unit but do not run `systemctl --user enable --now`.
  --no-web      Skip building the Dioxus web client; the daemon then embeds a
                placeholder page instead of the real UI.
  --no-github-token
                Do not seed GITHUB_TOKEN in ~/.config/favetto/env from `gh`.
  -h, --help    Show this help and exit.

The script builds the Dioxus web client into web/dist (so the daemon embeds it),
builds and installs the `favetto` binary with cargo, seeds
~/.config/favetto/config.toml from config.example.toml when missing, creates the
private ~/.config/favetto/env secrets file (mode 600, loaded by the unit via
EnvironmentFile=), installs the unit from packaging/systemd/favetto.service,
reloads the user manager and restarts the service so it runs with that
environment.
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
no_web=0
seed_github_token=1

while [ "$#" -gt 0 ]; do
    case "$1" in
        --uninstall) uninstall=1 ;;
        --purge) purge=1 ;;
        --no-start) no_start=1 ;;
        --no-web) no_web=1 ;;
        --no-github-token) seed_github_token=0 ;;
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

# True once the unit reports `active`. `restart` returns before systemd has
# finished flipping the state, so retry briefly.
service_active() {
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        if systemctl --user is-active --quiet "$SERVICE_NAME"; then
            return 0
        fi
        sleep 0.2
    done
    return 1
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

# Build the Dioxus web client into `web/dist` so the daemon embeds the real UI.
#
# `rust-embed` reads `web/dist` at proc-macro expansion time, so the bundle must
# exist before `cargo install` compiles the daemon. Skipped with `--no-web`.
build_web_client() {
    web_dir="$repo_root/web"

    if [ ! -f "$web_dir/Dioxus.toml" ]; then
        warn "no web client sources at $web_dir; the daemon will embed the placeholder page"
        return 0
    fi

    if ! command -v dx >/dev/null 2>&1; then
        die "dioxus-cli (dx) not found on PATH, needed to build the web client.
       Install it with: cargo install dioxus-cli --version 0.7.10 --locked
       Or re-run with --no-web to install the daemon without the embedded UI."
    fi

    if command -v rustup >/dev/null 2>&1 &&
        ! rustup target list --installed 2>/dev/null | grep -qx 'wasm32-unknown-unknown'; then
        log "adding the wasm32-unknown-unknown Rust target"
        rustup target add wasm32-unknown-unknown
    fi

    log "building the web client (dx bundle --release --platform web)"
    (
        cd "$web_dir"
        dx bundle --locked --release --platform web
    )

    # Dioxus 0.7 emits the bundle under dist/public; flatten it into dist/ so
    # rust-embed finds index.html and assets/ at its crate-relative path.
    rm -rf "$web_dir/dist/assets"
    cp -R "$web_dir/dist/public/." "$web_dir/dist/"
    rm -rf "$web_dir/dist/public"

    if [ ! -f "$web_dir/dist/index.html" ]; then
        die "web client build did not produce $web_dir/dist/index.html"
    fi
    log "web client built into $web_dir/dist"
}

# Create the daemon's private env file (mode 600) if it does not exist. The unit
# loads it with `EnvironmentFile=`, so scheduled/action tasks can interpolate
# secrets such as `GITHUB_TOKEN` without them living in the unit or the config.
#
# When `gh` is installed and authenticated and `GITHUB_TOKEN` is not already set
# in the file, seed it from `gh auth token` (unless --no-github-token was passed).
# An existing value is never overwritten.
ensure_env_file() {
    if [ -f "$env_file" ]; then
        log "keeping existing $env_file"
    else
        mkdir -p "$cfg_dir"
        {
            printf '# favetto daemon secrets, loaded by the systemd unit via\n'
            printf '# EnvironmentFile (see packaging/systemd/favetto.service).\n'
            printf '# One KEY=value per line; keep this file at mode 600.\n'
            printf '#\n'
            printf '# GITHUB_TOKEN=ghp_...\n'
            printf '# GITHUB_WEBHOOK_SECRET=...\n'
        } >"$env_file"
        chmod 600 "$env_file"
        log "created $env_file (mode 600)"
    fi

    if [ "$seed_github_token" -eq 1 ] &&
        ! grep -qE '^[[:space:]]*GITHUB_TOKEN=' "$env_file" &&
        command -v gh >/dev/null 2>&1; then
        if gh_token="$(gh auth token 2>/dev/null)" && [ -n "$gh_token" ]; then
            printf 'GITHUB_TOKEN=%s\n' "$gh_token" >>"$env_file"
            chmod 600 "$env_file"
            log "seeded GITHUB_TOKEN in $env_file from 'gh auth token'"
        fi
    fi
}

install_service() {
    [ -f "$unit_src" ] || die "unit template not found: $unit_src"

    if ! command -v cargo >/dev/null 2>&1; then
        die "cargo not found on PATH; install Rust from https://rustup.rs first"
    fi

    if [ "$no_web" -eq 1 ]; then
        log "skipping the web client build (--no-web)"
    else
        build_web_client
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

    # The unit loads this file with EnvironmentFile=, so it must exist (and be
    # populated) before the service is (re)started below.
    ensure_env_file

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
        systemctl --user enable "$SERVICE_NAME"
        # Reload first so `restart` picks up the freshly written unit, including
        # its EnvironmentFile= line. `restart` alone reuses the unit the manager
        # already has cached and would keep the previous environment.
        systemctl --user daemon-reload
        # `restart` (not `enable --now`) so an already-running daemon is moved
        # onto the freshly installed binary and re-reads its environment —
        # `enable --now` is a no-op for an active unit, so it would keep serving
        # the previous build and the previous environment.
        systemctl --user restart "$SERVICE_NAME"

        if service_active; then
            log "enabled and restarted $SERVICE_NAME; daemon runs with $env_file"
        else
            warn "$SERVICE_NAME did not stay active; check:"
            warn "  journalctl --user -u $SERVICE_NAME -e"
        fi
    fi

    cat <<EOF

favetto is installed as a systemd user service.

  status:  systemctl --user status $SERVICE_NAME
  logs:    journalctl --user -u $SERVICE_NAME -f
  restart: systemctl --user restart $SERVICE_NAME
  stop:    systemctl --user stop $SERVICE_NAME
  attach:  favetto tui

The daemon serves the embedded web client at its listen address (the
[daemon].listen setting in $cfg_file); open that URL in a browser to sign in.

Agent CLIs must be reachable from the unit's PATH; edit the Environment=PATH
line in $unit_dst if they live elsewhere.

Task secrets live in $env_file (mode 600) and are loaded by the unit through
EnvironmentFile. Add or edit KEY=value lines there (for example GITHUB_TOKEN),
then run 'systemctl --user restart $SERVICE_NAME' to reload the environment.
EOF
}

if [ "$uninstall" -eq 1 ]; then
    uninstall_service
else
    install_service
fi
