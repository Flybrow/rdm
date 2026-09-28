#!/bin/sh
# Signs the update packages of a release: sh .github/sign-packages.sh <version> <folder>
# The key is the PEM private key in $RDM_SIGNING_KEY (the repository secret). Each package gets
# `<package>.sig`: an Ed25519 signature of exactly what update.rs's signed_statement() rebuilds —
# the version, the package's name and its SHA-256 — checked here with the public key before going out.
set -eu

version=$1
dir=$2
[ -n "${RDM_SIGNING_KEY:-}" ] || { echo "::error::secret RDM_SIGNING_KEY is missing: RDM would refuse this release's updates"; exit 1; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
key="$work/key.pem"
(umask 077 && printf '%s\n' "$RDM_SIGNING_KEY" >"$key")

# The key must be the one RDM trusts.
public=$(openssl pkey -in "$key" -pubout -outform DER | tail -c 32 | od -An -tx1 | tr -d ' \n')
grep -q "\"$public\"" crates/app/src/update.rs || { echo "::error::RDM_SIGNING_KEY does not match update.rs's PUBLIC_KEY"; exit 1; }
openssl pkey -in "$key" -pubout -out "$work/key.pub"

signed=0
for f in "$dir"/*.msi "$dir"/*.deb "$dir"/*.rpm "$dir"/*.tar.gz; do
    [ -e "$f" ] || continue
    printf 'rdm-update\nversion=%s\nname=%s\nsha256=%s\n' "$version" "$(basename "$f")" "$(sha256sum "$f" | cut -d' ' -f1)" >"$work/statement"
    openssl pkeyutl -sign -inkey "$key" -rawin -in "$work/statement" -out "$f.sig"
    openssl pkeyutl -verify -pubin -inkey "$work/key.pub" -rawin -in "$work/statement" -sigfile "$f.sig" >/dev/null
    echo "signed $(basename "$f")"
    signed=$((signed + 1))
done
[ "$signed" -gt 0 ] || { echo "::error::no package to sign in $dir"; exit 1; }
