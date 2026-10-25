#!/usr/bin/env bash
# kurin installer: curl -fsSL https://raw.githubusercontent.com/weironz/kurin/main/scripts/install.sh | bash
set -euo pipefail

REPO="${KURIN_REPO:-weironz/kurin}"
VERSION="${KURIN_VERSION:-latest}"
PREFIX="${KURIN_PREFIX:-/usr/local/bin}"

case "$(uname -m)" in
  x86_64|amd64)  ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac

BASE="${KURIN_BASE_URL:-https://github.com/$REPO/releases}"
if [ "$VERSION" = "latest" ]; then
  URL="$BASE/latest/download/kurin-$ARCH-linux-musl.tar.gz"
else
  URL="$BASE/download/$VERSION/kurin-$ARCH-linux-musl.tar.gz"
fi
SUM_URL="$URL.sha256"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

name="kurin-$ARCH-linux-musl.tar.gz"
echo "[install] $URL"
curl -fsSL "$URL" -o "$tmp/$name"
curl -fsSL "$SUM_URL" -o "$tmp/$name.sha256"
( cd "$tmp" && sha256sum -c "$name.sha256" )

tar -xzf "$tmp/$name" -C "$tmp"
mkdir -p "$PREFIX"
install -m755 "$tmp/kurin" "$PREFIX/kurin"
echo "[install] kurin $VERSION -> $PREFIX/kurin"
"$PREFIX/kurin" create config --print-versions || true
