#!/usr/bin/env bash
# Downloads the prebuilt phantun binaries (static musl) into bin/<arch>/ and
# checks them against bin/SHA256SUMS. Run once before scripts/image.sh.
set -euo pipefail
cd "$(dirname "$0")/.."
version="v0.8.1"
base="https://github.com/dndx/phantun/releases/download/$version"
declare -A asset=(
  [amd64]=x86_64-unknown-linux-musl
  [arm64]=aarch64-unknown-linux-musl
  [armv7]=armv7-unknown-linux-musleabihf
)
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
for arch in amd64 arm64 armv7; do
  if [ -x "bin/$arch/phantun_client" ] && [ -x "bin/$arch/phantun_server" ]; then
    continue
  fi
  echo "fetching phantun $version for $arch"
  curl -fsSL --max-time 180 -o "$work/$arch.zip" "$base/phantun_${asset[$arch]}.zip"
  mkdir -p "bin/$arch"
  python3 -I -c 'import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])' "$work/$arch.zip" "bin/$arch"
  chmod 0755 "bin/$arch/phantun_client" "bin/$arch/phantun_server"
done
(cd bin && sha256sum -c SHA256SUMS)
