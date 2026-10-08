#!/usr/bin/env bash
# Build .deb, .rpm and Arch (.pkg.tar.zst) packages for kestrel.
#
# Packaging runs inside a Debian container so the host only needs cargo + docker.
# Output lands in the repo root:
#   kestrel_<ver>_amd64.deb
#   kestrel-<ver>-1.x86_64.rpm
#   kestrel-<ver>-1-x86_64.pkg.tar.zst
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DESCRIPTION="Post to X from the CLI or via MCP server for AI agents"
MAINTAINER="Ninepoint Labs"
URL="https://github.com/ninepointlabs/kestrel"

cargo build --release
test -x target/release/kestrel

rm -rf pkg
mkdir -p pkg/deb/DEBIAN pkg/deb/usr/local/bin
install -m 0755 target/release/kestrel pkg/deb/usr/local/bin/kestrel
sed "s/@VERSION@/$VERSION/" packaging/deb/control > pkg/deb/DEBIAN/control

docker run --rm \
  -v "$PWD:/src" -w /src \
  -e VERSION="$VERSION" -e DESCRIPTION="$DESCRIPTION" \
  -e MAINTAINER="$MAINTAINER" -e URL="$URL" \
  -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
  -e DEBIAN_FRONTEND=noninteractive \
  debian:trixie bash -euo pipefail -c '
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      ruby ruby-dev build-essential rpm libarchive-tools zstd >/dev/null
    gem install --no-document fpm >/dev/null

    dpkg-deb --root-owner-group --build pkg/deb "kestrel_${VERSION}_amd64.deb"

    for target in rpm pacman; do
      fpm -s dir -t "$target" -f \
        -n kestrel -v "$VERSION" --iteration 1 -a x86_64 \
        --license MIT --maintainer "$MAINTAINER" --vendor "$MAINTAINER" \
        --url "$URL" --description "$DESCRIPTION" \
        target/release/kestrel=/usr/local/bin/kestrel
    done

    chown "$HOST_UID:$HOST_GID" kestrel_* kestrel-*
    chown -R "$HOST_UID:$HOST_GID" pkg
  '

ls -l "kestrel_${VERSION}_amd64.deb" \
      "kestrel-${VERSION}-1.x86_64.rpm" \
      "kestrel-${VERSION}-1-x86_64.pkg.tar.zst"
