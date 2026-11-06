#!/usr/bin/env bash
# Native builds and temporary CLI validation, following werk1112; see NOTICE.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
require_command() { command -v "$1" >/dev/null 2>&1 || die "Required command not found: $1"; }

case "${1:-}" in
  linux) target=x86_64-unknown-linux-gnu; alias=build-linux; host=Linux:x86_64 ;;
  macos) target=aarch64-apple-darwin; alias=build-macos-apple-silicon; host=Darwin:arm64 ;;
  *) echo 'Usage: scripts/build-unix.sh <linux|macos>' >&2; exit 2 ;;
esac
platform=$1
shift
if [[ "$#" == 1 && ( "$1" == --help || "$1" == -h ) ]]; then
  printf 'Usage: scripts/build-%s.sh\nBuilds both release commands on %s and validates their CLI reference.\n' "$platform" "$host"
  exit 0
fi
[[ "$#" == 0 ]] || die "Usage: scripts/build-$platform.sh"
[[ "$(uname -s):$(uname -m)" == "$host" ]] || die "Build $platform on its native host ($host)."
for command in cargo rustc python3; do require_command "$command"; done
python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 11) else "Python 3.11+ is required")'
rust_host=$(rustc -vV | sed -n 's/^host: //p')
[[ "$rust_host" == "$target" ]] || die "Use the native $target Rust toolchain (found $rust_host)."
if [[ "$platform" == linux ]]; then
  require_command cc
else
  require_command xcrun
  xcrun --find clang >/dev/null || die 'Install the Xcode command-line tools with xcode-select --install.'
fi

cargo "$alias"
build="${CARGO_TARGET_DIR:-target}/$target/release"
for binary in werk-worker ww; do
  [[ -x "$build/$binary" ]] || die "Expected executable not found: $build/$binary"
  "$build/$binary" --version
done
reference=$(mktemp -d)
trap 'rm -rf -- "$reference"' EXIT
python3 scripts/generate-cli-reference.py "$build/werk-worker" --output-dir "$reference"
printf 'Built and validated %s/werk-worker and %s/ww\n' "$build" "$build"
