#!/bin/sh
# RDM on Ubuntu 26.04, in one command:      sh install-ubuntu.sh
# Uninstall:                                sh install-ubuntu.sh --uninstall
#
# 1. installs the build tools and, if needed, Rust (rustup, in ~/.cargo);
# 2. builds an optimized RDM;
# 3. installs it for your account (~/.local, no administrator rights for RDM itself):
#    in the GNOME applications with its icon, started at every login
#    (minimized to the notification area);
# 4. starts it.
# sudo is only used for missing system packages (apt).
set -eu
cd "$(dirname "$0")"

say() { printf '\033[1;35m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }

if [ "${1:-}" = "--uninstall" ]; then
    exec sh packaging/linux/install.sh --uninstall
fi

if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
fi
[ "${ID:-}" = ubuntu ] || warn "made for Ubuntu 26.04 (this system: ${PRETTY_NAME:-unknown}); going on"

say "Build tools (sudo may ask for your password)"
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential pkg-config curl ca-certificates >/dev/null

# Rust: the project's minimum version (rust-version in Cargo.toml), through rustup.
need=$(sed -n 's/^rust-version *= *"\([0-9.]*\)"/\1/p' Cargo.toml)
# `cargo 1.90.0 (…)` → true if at least $need; false when cargo is missing.
recent_enough() {
    version=$("$1" --version 2>/dev/null | cut -d' ' -f2)
    [ -n "$version" ] || return 1
    major=${version%%.*} rest=${version#*.}
    minor=${rest%%.*}
    need_major=${need%%.*} need_rest=${need#*.}
    need_minor=${need_rest%%.*}
    [ "$major" -gt "$need_major" ] || { [ "$major" -eq "$need_major" ] && [ "$minor" -ge "$need_minor" ]; }
}
cargo=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")
if ! recent_enough "$cargo"; then
    if [ -x "$HOME/.cargo/bin/rustup" ]; then
        say "Updating Rust"
        "$HOME/.cargo/bin/rustup" update stable
    else
        say "Installing Rust (rustup, in ~/.cargo)"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    fi
    cargo="$HOME/.cargo/bin/cargo"
fi

say "Building RDM (a few minutes the first time)"
"$cargo" build --release -p rdm

say "Installing: GNOME applications, icon, start at login"
sh packaging/linux/install.sh --autostart

# An RDM already open was closed cleanly then started again by install.sh (or reported if it is
# too old to close on request). The browsers' connectors run the same binary: not counted.
if ! pgrep -a -x rdm 2>/dev/null | grep -v -e 'chrome-extension://' -e 'rdm@rdm-download-manager' -e 'rdm.bridge.json' | grep -q .; then
    say "Starting RDM"
    nohup "$HOME/.local/bin/rdm" >/dev/null 2>&1 &
fi
echo
echo "RDM is installed: look for \"RDM\" in the applications (Super key), or the Desktop icon."
echo "Browser extension: in RDM, the \"Browser extension\" card (Firefox, Waterfox, Chrome, Brave, Opera, Edge)."
