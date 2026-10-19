#!/usr/bin/env bash
# Adapted from werk1112's platform packaging surface; see NOTICE.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
case "${1:-}" in
  linux) target=x86_64-unknown-linux-gnu; platform=linux-x86_64; alias=build-linux; host=Linux:x86_64 ;;
  linux-aarch64) target=aarch64-unknown-linux-gnu; platform=linux-aarch64; alias=build-linux-aarch64; host=Linux:aarch64 ;;
  macos) target=aarch64-apple-darwin; platform=macos-aarch64; alias=build-macos-apple-silicon; host=Darwin:arm64 ;;
  windows) echo 'Use scripts/package-release.ps1 in native Windows Developer PowerShell.' >&2; exit 2 ;;
  *) echo 'Usage: scripts/package-release.sh <linux|linux-aarch64|macos>' >&2; exit 2 ;;
esac
actual="$(uname -s):$(uname -m)"
[[ "$actual" == "$host" || "$host:$actual" == Linux:aarch64:Linux:arm64 ]] || { echo "Build $platform on its native host ($host)." >&2; exit 1; }
version=$(awk -F '"' '/^\[package\]/{p=1;next} /^\[/{p=0} p && /^version = /{print $2;exit}' Cargo.toml)
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 1
cargo "$alias"
build="${CARGO_TARGET_DIR:-target}/$target/release"
staging=$(mktemp -d)
trap 'rm -rf -- "$staging"' EXIT
python3 scripts/generate-cli-reference.py "$build/werk-worker" --output-dir "$staging/reference"
for binary in werk-worker ww; do
  "$build/$binary" --version
  cp "$build/$binary" "$staging/$binary"
done
cp README.md LICENSE NOTICE "$staging/"
mkdir -p releases
artifact="werkWorker-v$version-$platform.tar.gz"
tar -czf "releases/$artifact" -C "$staging" werk-worker ww README.md LICENSE NOTICE
(cd releases; if command -v sha256sum >/dev/null; then sha256sum "$artifact"; else shasum -a 256 "$artifact"; fi) > "releases/$artifact.sha256"
printf 'Created releases/%s and its .sha256\n' "$artifact"
