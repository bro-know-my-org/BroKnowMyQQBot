#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

: "${RELEASE_TAG:?version tag required}"
: "${SOURCE_REVISION:?source revision required}"
: "${SOURCE_DATE_EPOCH:?commit timestamp required}"
[[ "$RELEASE_TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]
[[ "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]]
[[ "$SOURCE_DATE_EPOCH" =~ ^[0-9]+$ ]]

binary="${1:-target/release/bkmqb}"
output="${2:-dist}"
version="${RELEASE_TAG#v}"
name="bkmqb-${RELEASE_TAG}-x86_64-unknown-linux-gnu"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging/$name" "$output"
install -m 755 "$binary" "$staging/$name/bkmqb"
strip --strip-unneeded "$staging/$name/bkmqb"
install -m 644 LICENSE-MIT LICENSE-APACHE "$staging/$name/"

python3 - "$staging/$name/bkmqb" "$version" "$SOURCE_REVISION" <<'PY'
import re
import subprocess
import sys

binary, version, revision = sys.argv[1:]
header = subprocess.check_output(["readelf", "-h", binary], text=True)
if not re.search(r"Machine:\s+Advanced Micro Devices X86-64", header):
    raise SystemExit("release binary must be Linux x86_64")
symbols = subprocess.check_output(["readelf", "--version-info", binary], text=True)
versions = [tuple(map(int, v.split("."))) for v in re.findall(r"GLIBC_([0-9.]+)", symbols)]
if not versions or max(versions) > (2, 36):
    raise SystemExit("release binary must require glibc <= 2.36")
print("Required glibc:", ".".join(map(str, max(versions))))
description = subprocess.check_output([binary, "--version"], text=True).strip()
if not description.startswith(f"bkmqb {version} (git {revision[:12]},"):
    raise SystemExit("binary version/source does not match the release tag")
print(description)
PY

tar --sort=name --mtime="@$SOURCE_DATE_EPOCH" --owner=0 --group=0 --numeric-owner \
  -C "$staging" -cf - "$name" | gzip -n > "$output/$name.tar.gz"
(
  cd "$output"
  sha256sum "$name.tar.gz" > SHA256SUMS
  sha256sum --check SHA256SUMS
)
