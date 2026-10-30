#!/bin/sh
# Explicit host setup; never invoked by the model or tool discovery.
set -eu
runtime="${WERK_WORKER_BROWSER_RUNTIME:-${XDG_DATA_HOME:-$HOME/.local/share}/werk-worker/tools/browser}"
local_deps=0
if [ "${1:-}" = "--local-deps" ]; then
    local_deps=1
    shift
fi
if [ "$#" -eq 0 ]; then
    set -- chromium firefox webkit
fi
for browser in "$@"; do
    case "$browser" in
        chromium|firefox|webkit) ;;
        *) printf '%s\n' 'Usage: install-browser.sh [--local-deps] [chromium firefox webkit]' >&2; exit 2 ;;
    esac
done
mkdir -p "$runtime"
/usr/bin/python3 -m pip install --upgrade --target "$runtime/python" 'playwright==1.58.0'
if [ "$local_deps" -eq 1 ]; then
    PYTHONPATH="$runtime/python" /usr/bin/python3 "$(dirname "$0")/install-browser-libs.py" "$runtime" "$@"
    multiarch="$(dpkg-architecture -qDEB_HOST_MULTIARCH)"
    export LD_LIBRARY_PATH="$runtime/deps/usr/lib/$multiarch:$runtime/deps/lib/$multiarch${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
PYTHONPATH="$runtime/python" PLAYWRIGHT_BROWSERS_PATH="$runtime/browsers" /usr/bin/python3 -m playwright install "$@"
printf '%s\n' "Browser runtime installed: $runtime"
