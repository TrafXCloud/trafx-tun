#!/usr/bin/env bash
# Builds a container image tar that both Docker and MikroTik RouterOS
# (/container add file=...) can load: the classic docker-archive layout with
# an uncompressed layer and "<sha>.json" config. RouterOS rejects the newer
# `docker save` layout ("no config found in manifest").
#
# Usage: scripts/image.sh [version] [amd64|arm64|armv7]
#   -> dist/trafx-phantun-<arch>.tar
set -euo pipefail
cd "$(dirname "$0")/.."
version="${1:-0.4.0}"
arch="${2:-amd64}"
case "$arch" in
  amd64|arm64) platform="linux/$arch" ;;
  armv7|arm) arch=armv7; platform="linux/arm/v7" ;;
  *) echo "unsupported arch $arch (amd64, arm64, armv7)" >&2; exit 2 ;;
esac
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
tag="trafx-phantun:${version}-${arch}"
[ -n "${NO_BUILD:-}" ] || docker build -q --platform "$platform" \
  --build-arg BASE_IMAGE="${BASE_IMAGE:-docker.arvancloud.ir/library/alpine:3.20}" \
  ${ALPINE_MIRROR:+--build-arg ALPINE_MIRROR="$ALPINE_MIRROR"} \
  -t "$tag" . >/dev/null
out="dist/trafx-phantun-${OUT_VERSION:+$OUT_VERSION-}${arch}.tar"
docker save "$tag" -o "$work/saved.tar"
python3 -I - "$work/saved.tar" "$out" <<'EOF'
import gzip, hashlib, io, json, sys, tarfile
src, dst = sys.argv[1], sys.argv[2]
with tarfile.open(src) as t:
    m = json.load(t.extractfile("manifest.json"))[0]
    cfg = t.extractfile(m["Config"]).read()
    layers = []
    for l in m["Layers"]:
        data = t.extractfile(l).read()
        if data[:2] == b"\x1f\x8b":
            data = gzip.decompress(data)
        layers.append(data)
diff_ids = json.loads(cfg)["rootfs"]["diff_ids"]
cfg_name = hashlib.sha256(cfg).hexdigest() + ".json"
files = {cfg_name: cfg}
names = []
for i, data in enumerate(layers):
    h = hashlib.sha256(data).hexdigest()
    assert "sha256:" + h == diff_ids[i], "layer digest mismatch"
    files[f"{h}/layer.tar"] = data
    files[f"{h}/VERSION"] = b"1.0"
    files[f"{h}/json"] = json.dumps({"id": h}).encode()
    names.append(f"{h}/layer.tar")
files["manifest.json"] = json.dumps([{"Config": cfg_name, "RepoTags": m["RepoTags"], "Layers": names}]).encode()
with tarfile.open(dst, "w", format=tarfile.USTAR_FORMAT) as out:
    for name in sorted(files):
        ti = tarfile.TarInfo(name)
        ti.size = len(files[name])
        ti.mode = 0o644
        out.addfile(ti, io.BytesIO(files[name]))
EOF
sha256sum "$out" | tee "$out.sha256"
echo "built $out"
