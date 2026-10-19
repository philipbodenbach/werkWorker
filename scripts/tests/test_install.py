import hashlib
import io
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile
import unittest

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

if __name__ == '__main__': unittest.main()
