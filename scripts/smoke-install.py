#!/usr/bin/env python3
"""Install a real native release archive into a temporary directory and run it."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib

root = Path(__file__).resolve().parent.parent
version = tomllib.loads((root / 'Cargo.toml').read_text())['package']['version']
with tempfile.TemporaryDirectory(prefix='werkWorker-install-smoke-') as tmp:
    tmp = Path(tmp)
    env = dict(os.environ, WERK_WORKER_VERSION=version,
               WERK_WORKER_ARCHIVE=str(Path(sys.argv[1]).resolve()),
               WERK_WORKER_INSTALL_DIR=str(tmp / 'bin'), WERK_WORKER_ADD_TO_PATH='0')
    command = ['pwsh', '-NoProfile', '-File', str(root / 'scripts/install.ps1')] if os.name == 'nt' else ['sh', str(root / 'scripts/install.sh')]
    subprocess.run(command, env=env, check=True)
    workspace = tmp / 'workspace'; workspace.mkdir()
    for name in ['werk-worker', 'ww']:
        binary = tmp / 'bin' / (name + ('.exe' if os.name == 'nt' else ''))
        output = subprocess.check_output([str(binary), '--help'], text=True, cwd=workspace)
        assert 'Usage: ' + name in output
        output = subprocess.check_output([str(binary), '--store', str(tmp / (name + '.sqlite')), 'run', '--demo', 'installation smoke'], text=True, cwd=workspace)
        assert 'offline demo completed' in output
        subprocess.run([str(binary), 'skills', 'inspect', 'ticket-drafting'], cwd=workspace, check=True, stdout=subprocess.DEVNULL)
    print('Both installed commands, embedded skills and offline sessions passed outside the checkout.')
