"""Exercise native build entrypoints and packaging with a disposable toolchain."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


@unittest.skipUnless(os.name == 'posix' and shutil.which('bash') and shutil.which('tar'),
                     'Unix build fixtures require POSIX bash and tar')
class NativeBuildTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='worker-build-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.repo = self.root / 'repo with spaces'
        scripts = self.repo / 'scripts'
        scripts.mkdir(parents=True)
        for name in ('build-unix.sh', 'build-linux.sh', 'build-macos.sh', 'package-release.sh'):
            shutil.copy2(ROOT / 'scripts' / name, scripts / name)
        (self.repo / 'Cargo.toml').write_text('[package]\nversion = "1.2.3"\n')
        for name in ('README.md', 'LICENSE', 'NOTICE'):
            (self.repo / name).write_text(name + ' fixture\n')
        (self.repo / 'docs').mkdir()
        (self.repo / 'docs/cli.md').write_text('canonical reference\n')
        self.trace = self.root / 'trace.jsonl'
        mockbin = self.root / 'mockbin'
        mockbin.mkdir()
        stubs = {
            'uname': "import os, sys\nprint(os.environ['FAKE_OS'] if sys.argv[1] == '-s' else os.environ['FAKE_ARCH'])\n",
            'rustc': "import os\nprint('host: ' + os.environ['FAKE_RUST_HOST'])\n",
            'cc': "pass\n",
            'xcrun': "print('/fixture/clang')\n",
            'cargo': '''import json, os, sys
from pathlib import Path
with open(os.environ['BUILD_TRACE'], 'a') as trace:
    trace.write(json.dumps(['cargo', *sys.argv[1:]]) + '\\n')
if os.environ.get('CARGO_FAIL'):
    sys.exit(17)
build = Path(os.environ.get('CARGO_TARGET_DIR', 'target')) / os.environ['FAKE_RUST_HOST'] / 'release'
build.mkdir(parents=True, exist_ok=True)
for name in ('werk-worker', 'ww'):
    binary = build / name
    binary.write_text('#!/bin/sh\\necho "' + name + ' 1.2.3"\\n')
    binary.chmod(0o755)
''',
        }
        for name, source in stubs.items():
            executable = mockbin / name
            executable.write_text('#!/usr/bin/env python3\n' + source)
            executable.chmod(0o755)
        (scripts / 'generate-cli-reference.py').write_text('''import json, os, sys
from pathlib import Path
assert Path(sys.argv[1]).is_file()
output = Path(sys.argv[sys.argv.index('--output-dir') + 1])
assert output.resolve() != Path('docs').resolve()
with open(os.environ['BUILD_TRACE'], 'a') as trace:
    trace.write(json.dumps(['reference', str(output)]) + '\\n')
if os.environ.get('REFERENCE_FAIL'):
    sys.exit(23)
(output / 'cli.md').write_text('temporary reference')
''')
        self.env = dict(os.environ, PATH=str(mockbin) + os.pathsep + os.environ['PATH'],
                        BUILD_TRACE=str(self.trace), FAKE_OS='Linux', FAKE_ARCH='x86_64',
                        FAKE_RUST_HOST='x86_64-unknown-linux-gnu',
                        CARGO_TARGET_DIR=str(self.root / 'custom target with spaces'))
        self.archive = self.repo / 'releases/werkWorker-v1.2.3-linux-x86_64.tar.gz'

    def run_script(self, name, *args):
        return subprocess.run(['bash', str(self.repo / 'scripts' / name), *args],
                              cwd=self.root, env=self.env, text=True, capture_output=True)

    def events(self):
        return [json.loads(line) for line in self.trace.read_text().splitlines()]

    def test_linux_build_from_outside_checkout_uses_custom_target_and_cleans_reference(self):
        result = self.run_script('build-linux.sh')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.events()[0], ['cargo', 'build-linux'])
        self.assertFalse(Path(self.events()[1][1]).exists())
        for binary in ('werk-worker', 'ww'):
            self.assertTrue((Path(self.env['CARGO_TARGET_DIR']) /
                             self.env['FAKE_RUST_HOST'] / 'release' / binary).is_file())
        self.assertEqual((self.repo / 'docs/cli.md').read_text(), 'canonical reference\n')
        self.assertFalse(self.archive.exists())

    def test_macos_uses_apple_silicon_build_and_validates_reference(self):
        self.env.update(FAKE_OS='Darwin', FAKE_ARCH='arm64', FAKE_RUST_HOST='aarch64-apple-darwin')
        result = self.run_script('build-macos.sh')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.events()[0], ['cargo', 'build-macos-apple-silicon'])
        self.assertFalse(Path(self.events()[1][1]).exists())

    def test_wrong_host_stops_before_build(self):
        result = self.run_script('build-macos.sh')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('native host', result.stderr)
        self.assertFalse(self.trace.exists())

    def test_wrong_rust_toolchain_stops_before_build(self):
        self.env['FAKE_RUST_HOST'] = 'aarch64-unknown-linux-gnu'
        result = self.run_script('build-linux.sh')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('native x86_64-unknown-linux-gnu Rust toolchain', result.stderr)
        self.assertFalse(self.trace.exists())

    def test_failed_build_or_reference_preserves_existing_archive(self):
        self.archive.parent.mkdir()
        self.archive.write_bytes(b'old archive')
        for failure in ('CARGO_FAIL', 'REFERENCE_FAIL'):
            with self.subTest(failure=failure):
                self.env[failure] = '1'
                result = self.run_script('package-release.sh', 'linux')
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.archive.read_bytes(), b'old archive')
                del self.env[failure]
        self.assertFalse(Path(self.events()[-1][1]).exists())

    def test_package_calls_build_once_and_writes_archive_with_checksum(self):
        result = self.run_script('package-release.sh', 'linux')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([event[0] for event in self.events()], ['cargo', 'reference'])
        with tarfile.open(self.archive) as archive:
            self.assertEqual(set(archive.getnames()), {'werk-worker', 'ww', 'README.md', 'LICENSE', 'NOTICE'})
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        self.assertEqual(Path(str(self.archive) + '.sha256').read_text().split(), [digest, self.archive.name])


@unittest.skipUnless(shutil.which('pwsh'), 'PowerShell parser is unavailable')
class WindowsBuildSyntaxTests(unittest.TestCase):
    def test_windows_build_and_package_scripts_parse(self):
        for name in ('build-windows.ps1', 'package-release.ps1'):
            with self.subTest(script=name):
                env = dict(os.environ, WORKER_PS_SCRIPT=str(ROOT / 'scripts' / name))
                result = subprocess.run(['pwsh', '-NoProfile', '-Command',
                    '$parseErrors = $null; $parseTokens = $null; '
                    '[System.Management.Automation.Language.Parser]::ParseFile('
                    '$env:WORKER_PS_SCRIPT, [ref]$parseTokens, [ref]$parseErrors) | Out-Null; '
                    'if ($parseErrors.Count) { $parseErrors | Out-String | Write-Error; exit 1 }'],
                    env=env, text=True, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == '__main__':
    unittest.main()
