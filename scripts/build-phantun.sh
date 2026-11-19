#!/usr/bin/env bash
# Builds the forked phantun (phantun/) as static musl binaries into bin/<arch>/,
# inside a Rust container (nothing is installed on the host).
#
# Usage: scripts/build-phantun.sh [amd64|arm64|armv7 ...]   (default: amd64)
set -euo pipefail
cd "$(dirname "$0")/.."
img="${RUST_IMAGE:-docker.arvancloud.ir/library/rust:1.91-alpine}"
archs=("${@:-amd64}")
docker volume create trafx-phantun-cargo >/dev/null
for arch in "${archs[@]}"; do
  case "$arch" in
    amd64) platform=linux/amd64 ;;
    arm64) platform=linux/arm64 ;;
    armv7) platform=linux/arm/v7 ;;
    *) echo "unsupported arch $arch" >&2; exit 2 ;;
  esac
  # Non-amd64 builds run under QEMU (binfmt): slow, but needs no cross toolchain.
  docker run --rm --platform "$platform" \
    -v "$PWD/phantun:/src" -v trafx-phantun-cargo:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/src/target/$arch -w /src "$img" \
    sh -c 'apk add --no-cache musl-dev >/dev/null && cargo build --release --locked -p phantun'
  mkdir -p "bin/$arch"
  for b in client server; do
    install -m 0755 "phantun/target/$arch/release/$b" "bin/$arch/phantun_$b"
  done
  echo "built bin/$arch/phantun_{client,server}"
done
