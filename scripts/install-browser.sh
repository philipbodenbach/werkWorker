#!/bin/sh
# Explicit host setup; never invoked by the model or tool discovery.
set -eu
runtime="${WERK_WORKER_BROWSER_RUNTIME:-${XDG_DATA_HOME:-$HOME/.local/share}/werk-worker/tools/browser}"
mkdir -p "$runtime"
/usr/bin/python3 -m pip install --upgrade --target "$runtime/python" 'playwright==1.58.0'
PYTHONPATH="$runtime/python" PLAYWRIGHT_BROWSERS_PATH="$runtime/browsers" /usr/bin/python3 -m playwright install chromium
printf '%s\n' "Browser runtime installed: $runtime"
