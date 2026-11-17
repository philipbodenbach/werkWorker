import hashlib
import io
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import tarfile
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[2]
@unittest.skipUnless(platform.system() == 'Linux', 'Unix installer adversarial tests run on Linux')
class InstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.archive = self.root / 'werkWorker-v0.1.0-linux-x86_64.tar.gz'
        self.env = dict(os.environ, WERK_WORKER_VERSION='0.1.0', WERK_WORKER_ARCHIVE=str(self.archive), WERK_WORKER_INSTALL_DIR=str(self.root/'bin'))
        # Test the install contract independently of the machine's CPU.
        mockbin = self.root / 'mock'; mockbin.mkdir()
        uname = mockbin / 'uname'; uname.write_text('#!/bin/sh\ncase "$1" in -s) echo Linux;; -m) echo x86_64;; esac\n'); uname.chmod(0o755)
        self.env['PATH'] = str(mockbin) + os.pathsep + os.environ['PATH']
    def archive_files(self, extra=None, symlink=False):
        with tarfile.open(self.archive, 'w:gz') as tar:
            for name in ['werk-worker','ww','README.md','LICENSE','NOTICE'] + ([extra] if extra else []):
                info = tarfile.TarInfo(name)
                data = b'#!/bin/sh\necho fixture\n'; info.size = len(data); info.mode=0o755
                if symlink and name == 'ww':
                    info.type=tarfile.SYMTYPE; info.linkname='/tmp/outside'; info.size=0
                tar.addfile(info, io.BytesIO(data) if info.isfile() else None)
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        Path(str(self.archive)+'.sha256').write_text(digest+'  '+self.archive.name+'\n')
    def install(self): return subprocess.run(['sh',str(ROOT/'scripts/install.sh')],env=self.env,capture_output=True,text=True)
    def test_both_commands_and_existing_state_preserved(self):
        self.archive_files(); (self.root/'bin').mkdir(); (self.root/'bin'/'session.sqlite').write_text('unchanged')
        result=self.install(); self.assertEqual(result.returncode,0,result.stderr)
        self.assertTrue((self.root/'bin'/'ww').stat().st_mode & 0o111)
        self.assertTrue((self.root/'bin'/'werk-worker').exists())
        self.assertEqual((self.root/'bin'/'session.sqlite').read_text(),'unchanged')
    def test_corrupt_checksum_never_replaces_existing_command(self):
        self.archive_files(); self.archive.write_bytes(self.archive.read_bytes()+b'corrupt')
        (self.root/'bin').mkdir(); (self.root/'bin'/'ww').write_text('old')
        self.assertNotEqual(self.install().returncode,0)
        self.assertEqual((self.root/'bin'/'ww').read_text(),'old')
    def test_traversal_member_is_rejected_before_install(self):
        self.archive_files('../escape'); self.assertNotEqual(self.install().returncode,0)
        self.assertFalse((self.root/'bin').exists())
    def test_symlink_member_is_rejected_before_install(self):
        self.archive_files(symlink=True); self.assertNotEqual(self.install().returncode,0)
        self.assertFalse((self.root/'bin').exists())
    def test_linux_arm64_reports_source_build_without_replacing_commands(self):
        (self.root/'bin').mkdir(); (self.root/'bin'/'ww').write_text('old')
        for arch in ('arm64', 'aarch64'):
            with self.subTest(arch=arch):
                (self.root/'mock'/'uname').write_text(
                    f'#!/bin/sh\ncase "$1" in -s) echo Linux;; -m) echo {arch};; esac\n')
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('no prebuilt Linux ARM64 archive', result.stderr)
                self.assertIn('cargo install --path . --locked', result.stderr)
                self.assertNotIn('Downloading', result.stdout)
                self.assertEqual((self.root/'bin'/'ww').read_text(), 'old')
    def test_macos_apple_silicon_uses_its_archive(self):
        self.archive = self.root/'werkWorker-v0.1.0-macos-aarch64.tar.gz'
        self.env['WERK_WORKER_ARCHIVE'] = str(self.archive)
        self.archive_files()
        (self.root/'mock'/'uname').write_text(
            '#!/bin/sh\ncase "$1" in -s) echo Darwin;; -m) echo arm64;; esac\n')
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(self.archive.name, result.stdout)
        self.assertTrue((self.root/'bin'/'ww').exists())
    def test_linux_arm64_is_not_an_official_package_target(self):
        result = subprocess.run(
            ['bash', str(ROOT/'scripts/package-release.sh'), 'linux-aarch64'],
            capture_output=True, text=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn('<linux|macos>', result.stderr)

@unittest.skipUnless(os.name == 'nt', 'Windows installer tests require native Windows')
class WindowsInstallTests(unittest.TestCase):
    binaries = ('werk-worker.exe', 'ww.exe', 'ww-windows-provision.exe', 'ww-windows-runner.exe')

    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='worker Windows installer ')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.archive = self.root / 'werkWorker-v0.3.3-windows-x86_64.zip'
        self.install_dir = self.root / 'installed commands'
        self.powershell = shutil.which('pwsh') or shutil.which('powershell.exe')
        self.assertIsNotNone(self.powershell)
        self.env = dict(os.environ, WERK_WORKER_VERSION='0.3.3',
                        WERK_WORKER_ARCHIVE=str(self.archive),
                        WERK_WORKER_INSTALL_DIR=str(self.install_dir),
                        WERK_WORKER_ADD_TO_PATH='0')

    def archive_files(self, omit=None, extra=None, symlink=None):
        names = [*self.binaries, 'README.md', 'LICENSE', 'NOTICE']
        if omit:
            names.remove(omit)
        if extra:
            names.append(extra)
        with zipfile.ZipFile(self.archive, 'w') as package:
            for name in names:
                entry = zipfile.ZipInfo(name)
                entry.external_attr = (stat.S_IFLNK if name == symlink else stat.S_IFREG) << 16
                package.writestr(entry, ('synthetic package member: ' + name).encode())
        digest = hashlib.sha256(self.archive.read_bytes()).hexdigest()
        Path(str(self.archive) + '.sha256').write_text(digest + '  ' + self.archive.name + '\n')

    def install(self):
        return subprocess.run([self.powershell, '-NoProfile', '-NonInteractive', '-File',
                               str(ROOT / 'scripts/install.ps1')], env=self.env,
                              capture_output=True, text=True, encoding='utf-8',
                              errors='replace', timeout=30)

    def test_all_four_commands_and_existing_state_preserved(self):
        self.archive_files()
        self.install_dir.mkdir()
        (self.install_dir / 'session.sqlite').write_bytes(b'unchanged')
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        with zipfile.ZipFile(self.archive) as package:
            for binary in self.binaries:
                self.assertEqual((self.install_dir / binary).read_bytes(), package.read(binary))
        self.assertEqual((self.install_dir / 'session.sqlite').read_bytes(), b'unchanged')

    def test_missing_helper_rejects_before_replacing_existing_commands(self):
        self.install_dir.mkdir()
        old = self.install_dir / 'ww.exe'
        old.write_bytes(b'existing command')
        for helper in self.binaries[2:]:
            with self.subTest(helper=helper):
                self.archive_files(omit=helper)
                result = self.install()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('Unexpected archive entries', result.stderr)
                self.assertEqual(old.read_bytes(), b'existing command')
                self.assertFalse((self.install_dir / 'werk-worker.exe').exists())

    def test_unsafe_or_duplicate_members_reject_before_install(self):
        for options in ({'extra': '../escape.exe'}, {'extra': self.binaries[2]},
                        {'symlink': self.binaries[3]}):
            with self.subTest(options=options):
                self.archive_files(**options)
                self.assertNotEqual(self.install().returncode, 0)
                self.assertFalse(self.install_dir.exists())
                self.assertFalse((self.root / 'escape.exe').exists())

    def test_corrupt_checksum_preserves_existing_commands(self):
        self.archive_files()
        self.archive.write_bytes(self.archive.read_bytes() + b'changed')
        self.install_dir.mkdir()
        old = self.install_dir / 'ww.exe'
        old.write_bytes(b'existing command')
        self.assertNotEqual(self.install().returncode, 0)
        self.assertEqual(old.read_bytes(), b'existing command')

    def test_helper_destination_directory_rejects_before_any_replacement(self):
        self.archive_files()
        self.install_dir.mkdir()
        (self.install_dir / 'ww-windows-runner.exe').mkdir()
        old = self.install_dir / 'ww.exe'
        old.write_bytes(b'existing command')
        self.assertNotEqual(self.install().returncode, 0)
        self.assertEqual(old.read_bytes(), b'existing command')
        self.assertFalse((self.install_dir / 'werk-worker.exe').exists())


if __name__ == '__main__': unittest.main()
