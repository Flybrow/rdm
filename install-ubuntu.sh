#!/bin/sh
# RDM sur Ubuntu 26.04, en une commande :      sh install-ubuntu.sh
# Désinstaller :                                sh install-ubuntu.sh --uninstall
#
# 1. installe les outils de compilation et, si besoin, Rust (rustup, dans ~/.cargo) ;
# 2. compile RDM en version optimisée ;
# 3. l'installe pour votre compte (~/.local, sans droits administrateur pour RDM lui-même) :
#    dans les applications GNOME avec son icône, et lancé à chaque ouverture de session
#    (réduit dans la zone de notification) ;
# 4. le démarre.
# sudo n'est utilisé que pour les paquets système manquants (apt).
set -eu
cd "$(dirname "$0")"

say() { printf '\033[1;35m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mattention :\033[0m %s\n' "$*" >&2; }

if [ "${1:-}" = "--uninstall" ]; then
    exec sh packaging/linux/install.sh --uninstall
fi

if [ -r /etc/os-release ]; then
    # shellcheck disable=SC1091
    . /etc/os-release
fi
[ "${ID:-}" = ubuntu ] || warn "prévu pour Ubuntu 26.04 (système : ${PRETTY_NAME:-inconnu}) ; on continue"

say "Outils de compilation (sudo peut demander votre mot de passe)"
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq build-essential pkg-config curl ca-certificates >/dev/null

# Rust : la version minimale du projet (rust-version dans Cargo.toml), via rustup.
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
        say "Mise à jour de Rust"
        "$HOME/.cargo/bin/rustup" update stable
    else
        say "Installation de Rust (rustup, dans ~/.cargo)"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    fi
    cargo="$HOME/.cargo/bin/cargo"
fi

say "Compilation de RDM (quelques minutes la première fois)"
"$cargo" build --release -p rdm

say "Installation : applications GNOME, icône, démarrage automatique"
sh packaging/linux/install.sh --autostart

if pgrep -x rdm >/dev/null 2>&1; then
    warn "RDM était déjà ouvert : quittez-le (icône de RDM → Quitter) puis relancez-le pour utiliser la nouvelle version."
else
    say "Démarrage de RDM"
    nohup "$HOME/.local/bin/rdm" >/dev/null 2>&1 &
fi
echo
echo "RDM est installé : cherchez « RDM » dans les applications (touche Super)."
echo "Extension du navigateur : voir README.md, section « Extension »."
