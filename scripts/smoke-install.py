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


WINDOWS_BINARIES = ('werk-worker.exe', 'ww.exe', 'ww-windows-provision.exe', 'ww-windows-runner.exe')


def sha256_stream(stream):
    digest = hashlib.sha256()
    for block in iter(lambda: stream.read(1024 * 1024), b''):
        digest.update(block)
    return digest.hexdigest()


def verify_windows_install(archive, install, build_bin=None):
    binaries = {}
    with zipfile.ZipFile(archive) as package:
        for name in WINDOWS_BINARIES:
            with package.open(name) as entry, (install / name).open('rb') as installed:
                hashes = {'archive': sha256_stream(entry), 'installed': sha256_stream(installed)}
            if build_bin is not None:
                with (build_bin / name).open('rb') as built:
                    hashes['build'] = sha256_stream(built)
            if len(set(hashes.values())) != 1:
                raise ValueError(f'Windows binary identity mismatch for {name}: {hashes}')
            binaries[name] = hashes
    return binaries


def smoke(root, archive, directory, windows_build_bin=None):
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
    if os.name == 'nt':
        report['binaries'] = verify_windows_install(archive, install, windows_build_bin)
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
        for name, expected_exit in [('ww-windows-provision.exe', 2), ('ww-windows-runner.exe', 125)]:
            # Request-free startup must refuse before provisioning or launching a payload.
            result = subprocess.run([str(install / name)], cwd=workspace, text=True, encoding='utf-8',
                                    errors='replace', capture_output=True, timeout=30)
            if result.returncode != expected_exit:
                raise RuntimeError(f'{name} returned {result.returncode}, expected {expected_exit}')
            report['helpers'].append({'name': name, 'exit_code': result.returncode,
                                      'sha256': report['binaries'][name]['installed']})
        for name, hashes in report['binaries'].items():
            with (install / name).open('rb') as current:
                hashes['after_smoke'] = sha256_stream(current)
            if hashes['after_smoke'] != hashes['archive']:
                raise ValueError(f'Installed binary changed during smoke: {name}')
    (directory / 'smoke-result.json').write_text(json.dumps(report, indent=2) + '\n')
    print('Installed CLI commands, embedded skills and offline sessions passed outside the checkout.')
    if os.name == 'nt':
        print('Both installed Windows helpers match the archive and refused unauthenticated startup.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=Path)
    parser.add_argument('--retain', type=Path,
                        help='Create and retain this fresh directory for explicit provisioning/start checks')
    parser.add_argument('--windows-build-bin', type=Path,
                        help='Also match all four installed Windows EXEs to this product build directory')
    args = parser.parse_args()
    if args.windows_build_bin is not None and os.name != 'nt':
        parser.error('--windows-build-bin requires a native Windows installation')
    root = Path(__file__).resolve().parent.parent
    archive = args.archive.resolve(strict=True)
    if args.retain is not None:
        directory = args.retain.absolute()
        directory.mkdir()  # Existing installations are never reused by a smoke.
        smoke(root, archive, directory, args.windows_build_bin)
        print(f'Retained fresh installation and result: {directory}')
    else:
        with tempfile.TemporaryDirectory(prefix='werkWorker-install-smoke-') as tmp:
            smoke(root, archive, Path(tmp), args.windows_build_bin)


if __name__ == '__main__':
    main()
