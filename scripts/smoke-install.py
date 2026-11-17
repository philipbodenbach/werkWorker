#!/usr/bin/env python3
"""Install a native release archive in a fresh directory and exercise its commands."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import zipfile


def package_version(path):
    # The native Windows build host may have Python 3.10. Read only the package
    # block and require one stable version; do not pick workspace/dependency data.
    package = re.search(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)", path.read_text())
    if package is None:
        raise ValueError("Missing Cargo package section")
    versions = re.findall(
        r'^version\s*=\s*"((?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*))"\s*(?:#.*)?$',
        package[1], re.MULTILINE,
    )
    if len(versions) != 1:
        raise ValueError("Expected one stable Cargo package version")
    return versions[0]


def smoke(root, archive, directory):
    version = package_version(root / 'Cargo.toml')
    install = directory / 'bin'
    env = dict(os.environ, WERK_WORKER_VERSION=version,
               WERK_WORKER_ARCHIVE=str(archive),
               WERK_WORKER_INSTALL_DIR=str(install), WERK_WORKER_ADD_TO_PATH='0')
    if os.name == 'nt':
        powershell = shutil.which('pwsh') or shutil.which('powershell.exe')
        if powershell is None:
            raise RuntimeError('Native Windows PowerShell or PowerShell 7 is required')
        command = [powershell, '-NoProfile', '-NonInteractive', '-File',
                   str(root / 'scripts/install.ps1')]
    else:
        command = ['sh', str(root / 'scripts/install.sh')]
    subprocess.run(command, env=env, check=True)
    workspace = directory / 'workspace'
    workspace.mkdir()
    report = {'archive': str(archive), 'install_dir': str(install), 'version': version,
              'commands': [], 'helpers': []}
    for name in ['werk-worker', 'ww']:
        binary = install / (name + ('.exe' if os.name == 'nt' else ''))
        output = subprocess.check_output([str(binary), '--help'], text=True, encoding='utf-8',
                                         errors='replace', cwd=workspace)
        assert 'Usage: ' + name in output
        output = subprocess.check_output(
            [str(binary), '--store', str(directory / (name + '.sqlite')),
             'run', '--demo', 'installation smoke'], text=True, encoding='utf-8',
            errors='replace', cwd=workspace,
        )
        assert 'offline demo completed' in output
        subprocess.run([str(binary), 'skills', 'inspect', 'ticket-drafting'],
                       cwd=workspace, check=True, stdout=subprocess.DEVNULL)
        report['commands'].append(name)
    if os.name == 'nt':
        with zipfile.ZipFile(archive) as package:
            for name in ('werk-worker.exe', 'ww.exe', 'ww-windows-provision.exe',
                         'ww-windows-runner.exe'):
                expected = hashlib.sha256(package.read(name)).hexdigest()
                actual = hashlib.sha256((install / name).read_bytes()).hexdigest()
                assert actual == expected, f'Installed binary differs from archive: {name}'
            for name, expected_exit in [('ww-windows-provision.exe', 2),
                                        ('ww-windows-runner.exe', 125)]:
                # No request/pipe arguments: verifies loading and refusal without
                # creating an account, changing ACLs or requesting elevation.
                result = subprocess.run([str(install / name)], cwd=workspace,
                                        text=True, encoding='utf-8', errors='replace',
                                        capture_output=True, timeout=30)
                assert result.returncode == expected_exit, (name, result)
                report['helpers'].append({'name': name, 'exit_code': result.returncode,
                                          'sha256': hashlib.sha256((install / name).read_bytes()).hexdigest()})
    (directory / 'smoke-result.json').write_text(json.dumps(report, indent=2) + '\n')
    print('Installed CLI commands, embedded skills and offline sessions passed outside the checkout.')
    if os.name == 'nt':
        print('Both installed Windows helpers match the archive and refused unauthenticated startup.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    parser.add_argument('--retain', type=Path,
                        help='Create and retain this fresh directory for explicit provisioning/start checks')
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    archive = args.archive.resolve(strict=True)
    if args.retain is not None:
        directory = args.retain.absolute()
        directory.mkdir()  # Existing installations are never reused by a smoke.
        smoke(root, archive, directory)
        print(f'Retained fresh installation and result: {directory}')
    else:
        with tempfile.TemporaryDirectory(prefix='werkWorker-install-smoke-') as tmp:
            smoke(root, archive, Path(tmp))


if __name__ == '__main__':
    main()
