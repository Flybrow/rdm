#!/bin/sh
# Per-user install: ./install.sh [--no-deps] [--autostart] [--uninstall]
# RDM itself goes to ~/.local (no root). The system libraries it needs (Wayland/X11, EGL,
# xkbcommon, desktop portal, tray support) are installed with sudo only when missing.
# --autostart: RDM starts at login, in the notification area.
# Supported: Debian/Ubuntu (apt), Fedora/RHEL (dnf), openSUSE (zypper), Arch (pacman).
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bin="$HOME/.local/bin"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
config="${XDG_CONFIG_HOME:-$HOME/.config}"
apps="$data/applications"
hicolor="$data/icons/hicolor"

deps=1
autostart=0

# Where RDM registers the browser extension's connector (native messaging host) at start.
native_hosts() {
    for d in .config/google-chrome/NativeMessagingHosts .config/google-chrome-beta/NativeMessagingHosts \
        .config/chromium/NativeMessagingHosts .config/BraveSoftware/Brave-Browser/NativeMessagingHosts \
        .config/microsoft-edge/NativeMessagingHosts .config/vivaldi/NativeMessagingHosts \
        .config/opera/NativeMessagingHosts .mozilla/native-messaging-hosts .waterfox/native-messaging-hosts \
        .librewolf/native-messaging-hosts; do
        printf '%s\n' "$HOME/$d/rdm.bridge.json"
    done
}

# Whether RDM itself runs (the browsers' connectors run the same binary: not counted).
rdm_running() {
    command -v pgrep >/dev/null || return 1
    pgrep -a -x rdm 2>/dev/null | grep -v -e 'chrome-extension://' -e 'rdm@rdm-download-manager' -e 'rdm.bridge.json' | grep -q .
}

# The user's Desktop folder (localized: "Bureau"…), empty when there is none.
desktop_folder() {
    dir=$(command -v xdg-user-dir >/dev/null && xdg-user-dir DESKTOP 2>/dev/null || true)
    [ -n "$dir" ] && [ "$dir" != "$HOME" ] || dir="$HOME/Desktop"
    [ -d "$dir" ] && printf '%s' "$dir"
    return 0
}

for arg in "$@"; do
    case "$arg" in
        --uninstall)
            dir=$(desktop_folder)
            [ -z "$dir" ] || rm -f "$dir/rdm.desktop"
            rm -f "$bin/rdm" "$apps/rdm.desktop" "$config/autostart/rdm.desktop"
            native_hosts | while read -r host; do rm -f "$host"; done
            find "$hicolor" -path '*/apps/rdm.*' -delete 2>/dev/null || true
            command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t "$hicolor" 2>/dev/null || true
            echo "RDM uninstalled (settings kept in $config/rdm)."
            exit 0
            ;;
        --no-deps) deps=0 ;;
        --autostart) autostart=1 ;;
        *) echo "usage: $0 [--no-deps] [--autostart] [--uninstall]" >&2; exit 2 ;;
    esac
done

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }

# ── Privileges (package installs only) ───────────────────────────────────
as_root() {
    if [ "$(id -u)" -eq 0 ]; then "$@"
    elif command -v sudo >/dev/null; then sudo "$@"
    elif command -v pkexec >/dev/null; then pkexec "$@"
    else warn "neither sudo nor pkexec: run as root: $*"; return 1
    fi
}

# ── Distribution & desktop ───────────────────────────────────────────────
distro=""
if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    distro=$(. /etc/os-release && echo "${ID:-} ${ID_LIKE:-}")
fi
desktop=$(printf '%s' "${XDG_CURRENT_DESKTOP:-}${DESKTOP_SESSION:-}" | tr '[:upper:]' '[:lower:]')

pm=""
case " $distro " in
    *" debian "* | *" ubuntu "*) pm=apt ;;
    *" fedora "* | *" rhel "* | *" centos "*) pm=dnf ;;
    *" suse "* | *" opensuse "*) pm=zypper ;;
    *" arch "*) pm=pacman ;;
esac

# Packages per manager. Everything below is loaded at run time (dlopen) by the GUI toolkit,
# so a missing one only shows when RDM starts: better to install it now.
#   window: xkbcommon, Wayland client/cursor/egl, X11 (fallback), EGL/GL (glow renderer)
#   file dialogs: xdg-desktop-portal + the backend of the desktop
#   notification-area icon: StatusNotifier host (built into KDE; an extension on GNOME)
runtime_packages() {
    case "$pm" in
        apt)
            echo libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libwayland-cursor0 libwayland-egl1 \
                libx11-6 libx11-xcb1 libxcursor1 libxrandr2 libxi6 libegl1 libgl1 libegl-mesa0 \
                dbus-user-session xdg-desktop-portal
            case "$desktop" in
                *kde* | *plasma*) echo xdg-desktop-portal-kde ;;
                *gnome* | *ubuntu*)
                    # Ubuntu ships the tray extension in its own bundle (the Debian name is only virtual there).
                    if apt-cache show gnome-shell-ubuntu-extensions >/dev/null 2>&1; then
                        echo xdg-desktop-portal-gnome gnome-shell-ubuntu-extensions
                    else
                        echo xdg-desktop-portal-gnome gnome-shell-extension-appindicator
                    fi
                    ;;
                *) echo xdg-desktop-portal-gtk ;;
            esac
            ;;
        dnf)
            echo libxkbcommon libxkbcommon-x11 libwayland-client libwayland-cursor libwayland-egl \
                libX11 libX11-xcb libXcursor libXrandr libXi libglvnd-egl libglvnd-glx mesa-libEGL mesa-libGL \
                xdg-desktop-portal
            case "$desktop" in
                *kde* | *plasma*) echo xdg-desktop-portal-kde ;;
                *gnome*) echo xdg-desktop-portal-gnome gnome-shell-extension-appindicator ;;
                *) echo xdg-desktop-portal-gtk ;;
            esac
            ;;
        zypper)
            echo libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libwayland-cursor0 libwayland-egl1 \
                libX11-6 libX11-xcb1 libXcursor1 libXrandr2 libXi6 libEGL1 libGL1 xdg-desktop-portal
            ;;
        pacman)
            echo libxkbcommon libxkbcommon-x11 wayland libx11 libxcursor libxrandr libxi libglvnd mesa xdg-desktop-portal
            ;;
    esac
}

build_packages() {
    case "$pm" in
        apt) echo build-essential pkg-config ;;
        dnf) echo gcc pkg-config ;;
        zypper) echo gcc pkg-config ;;
        pacman) echo base-devel ;;
    esac
}

is_installed() {
    case "$pm" in
        apt) dpkg-query -W -f='${Status}' "$1" 2>/dev/null | grep -q 'ok installed' ;;
        dnf | zypper) rpm -q "$1" >/dev/null 2>&1 ;;
        pacman) pacman -Q "$1" >/dev/null 2>&1 ;;
    esac
}

is_available() {
    case "$pm" in
        apt) apt-cache show "$1" >/dev/null 2>&1 ;;
        *) true ;; # dnf/zypper/pacman skip unknown names themselves (see flags below)
    esac
}

install_packages() {
    missing=""
    for p in "$@"; do
        is_installed "$p" || missing="$missing $p"
    done
    [ -n "$missing" ] || return 0
    say "Installing system packages:$missing"
    case "$pm" in
        apt)
            as_root apt-get update -qq || true
            available=""
            for p in $missing; do
                if is_available "$p"; then available="$available $p"; else warn "package $p not found, skipped"; fi
            done
            [ -z "$available" ] || as_root env DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends $available
            ;;
        dnf) as_root dnf install -y --skip-unavailable $missing ;;
        zypper) as_root zypper --non-interactive install --no-recommends $missing ;;
        pacman) as_root pacman -S --needed --noconfirm $missing ;;
    esac
}

if [ "$deps" -eq 1 ]; then
    if [ -n "$pm" ]; then
        # shellcheck disable=SC2046
        install_packages $(runtime_packages) || warn "some system packages could not be installed"
    else
        warn "unknown distribution: install Wayland/X11, EGL, xkbcommon and xdg-desktop-portal yourself"
    fi
fi

# ── Binary: shipped next to this script, or built from the source tree ──────
exe=""
for candidate in "$here/rdm" "$here/../../target/release/rdm"; do
    [ -x "$candidate" ] && exe=$candidate && break
done
if [ -z "$exe" ] && [ -f "$here/../../Cargo.toml" ]; then
    cargo=$(command -v cargo || echo "$HOME/.cargo/bin/cargo")
    if [ -x "$cargo" ]; then
        # shellcheck disable=SC2046
        [ "$deps" -eq 0 ] || [ -z "$pm" ] || install_packages $(build_packages)
        say "Building RDM (cargo build --release)…"
        (cd "$here/../.." && "$cargo" build --release -p rdm)
        exe="$here/../../target/release/rdm"
    fi
fi
[ -n "$exe" ] && [ -x "$exe" ] || {
    echo "rdm binary not found: put it next to this script, or install Rust (https://rustup.rs) and rerun" >&2
    exit 1
}

# ── Install (per user) ───────────────────────────────────────────────────
# A running RDM is closed cleanly first (downloads saved), by the new binary — an older one would
# not know the option — and started again afterwards, in its new version.
was_running=0
if rdm_running; then
    was_running=1
    "$exe" --quit >/dev/null 2>&1 || true
fi
mkdir -p "$bin" "$apps"
install -m 755 "$exe" "$bin/rdm"
# Absolute Exec: ~/.local/bin is not on the desktop session's PATH everywhere.
sed "s|^Exec=rdm |Exec=\"$bin/rdm\" |" "$here/rdm.desktop" >"$apps/rdm.desktop"
chmod 644 "$apps/rdm.desktop"
# Every size (16 → 512 px) plus the SVG: sharp in the launcher, the dock and the top bar.
if [ -d "$here/icons/hicolor" ]; then
    (cd "$here/icons/hicolor" && find . -path '*/apps/rdm.*' -type f) | while read -r icon; do
        install -D -m 644 "$here/icons/hicolor/$icon" "$hicolor/$icon"
    done
else
    for icon in "$here/rdm.png" "$here/../../crates/app/assets/rdm.png"; do
        [ -f "$icon" ] && install -D -m 644 "$icon" "$hicolor/256x256/apps/rdm.png" && break
    done
fi
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t -f "$hicolor" 2>/dev/null || true

# ── Desktop shortcut, where the desktop shows icons (a Desktop folder exists) ─
desktop_dir=$(desktop_folder)
if [ -n "$desktop_dir" ] && [ -d "$desktop_dir" ]; then
    install -m 755 "$apps/rdm.desktop" "$desktop_dir/rdm.desktop"
    # GNOME (Desktop Icons NG) launches only entries marked trusted.
    command -v gio >/dev/null && gio set "$desktop_dir/rdm.desktop" metadata::trusted true 2>/dev/null || true
    say "Desktop shortcut: $desktop_dir/rdm.desktop"
fi

# ── Start at login (same entry and flag as the app's own setting) ───────────
if [ "$autostart" -eq 1 ]; then
    mkdir -p "$config/autostart" "$config/rdm"
    cat >"$config/autostart/rdm.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=RDM
Exec="$bin/rdm" --minimized
Icon=rdm
X-GNOME-Autostart-enabled=true
EOF
    # Tick "Start with the system" in RDM's settings too (created, or updated in place).
    settings="$config/rdm/settings.json"
    if [ ! -s "$settings" ]; then
        printf '{"autostart":true}\n' >"$settings"
    elif command -v python3 >/dev/null; then
        python3 - "$settings" <<'EOF'
import json, sys
path = sys.argv[1]
with open(path, encoding="utf-8") as f:
    settings = json.load(f)
settings["autostart"] = True
with open(path, "w", encoding="utf-8") as f:
    json.dump(settings, f, ensure_ascii=False)
EOF
    fi
    say "RDM will start at login (in the notification area)"
fi

# ── Checks ───────────────────────────────────────────────────────────────
if command -v ldd >/dev/null && ldd "$bin/rdm" | grep -q 'not found'; then
    warn "missing libraries:"; ldd "$bin/rdm" | grep 'not found' >&2
fi
if command -v ldconfig >/dev/null || [ -x /sbin/ldconfig ]; then
    libs=$( (command -v ldconfig >/dev/null && ldconfig -p) || /sbin/ldconfig -p)
    for so in libxkbcommon.so.0 libEGL.so.1 libwayland-client.so.0 libX11.so.6; do
        printf '%s' "$libs" | grep -q "$so" || warn "$so not found: RDM's window may not open"
    done
fi
case "$desktop" in
    *gnome*)
        command -v gnome-extensions >/dev/null &&
            ! gnome-extensions list --enabled 2>/dev/null | grep -qi appindicator &&
            warn "tray icon on GNOME: enable the AppIndicator extension (Extensions app; 'gnome-extensions list | grep -i appindicator' gives its name), then log out and back in" || true
        ;;
esac
case ":$PATH:" in
    *":$bin:"*) ;;
    *) warn "$bin is not in your PATH (the menu entry works anyway)" ;;
esac

if [ "$was_running" -eq 1 ]; then
    if rdm_running; then
        warn "the previous RDM is still running (too old to close on request): quit it (tray icon → Quit) and start it again"
    else
        say "Starting the new RDM"
        nohup "$bin/rdm" >/dev/null 2>&1 &
    fi
fi

say "RDM installed to $bin/rdm"
echo "    Browser extension (Firefox, Waterfox, Chrome, Brave, Opera, Edge): in RDM, the \"Browser extension\" card."
