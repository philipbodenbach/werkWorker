import datetime
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('release', ROOT / 'scripts/prepare-release.py')
release = importlib.util.module_from_spec(spec); spec.loader.exec_module(release)

class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(); self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        files = {
            'Cargo.toml': '[package]\nname = "werk-worker"\nversion = "0.1.0"\n',
            'Cargo.lock': '[[package]]\nname = "werk-worker"\nversion = "0.1.0"\n',
            'examples/sdk-consumer/Cargo.lock': '[[package]]\nname = "werk-worker"\nversion = "0.1.0"\n',
            'README.md': '# werkWorker\n\n## What’s new in v0.1.0\n\nwerkWorker SDK and both CLI commands share release version **0.1.0**.\n',
            'CHANGELOG.md': '# Changelog\n\n## [Unreleased]\n\n- Initial version.\n',
            'docs/releases.md': 'Current release: `v0.1.0`.\n',
        }
        for name, content in files.items():
            p = self.root / name; p.parent.mkdir(parents=True, exist_ok=True); p.write_text(content)
        self.date = datetime.date(2026, 10, 19)
    def test_initial_release_then_retry_without_version_drift(self):
        with patch.object(release, 'git', return_value=''):
            tag, notes = release.prepare(self.root, 'auto', self.date)
            self.assertEqual(tag, 'v0.1.0')
            self.assertIn('Initial', notes)
            before = (self.root / 'CHANGELOG.md').read_text()
            self.assertEqual(release.prepare(self.root, 'auto', self.date)[0], tag)
            self.assertEqual((self.root / 'CHANGELOG.md').read_text(), before)
        def git(*args):
            return 'v0.1.0' if args[0] == 'tag' else 'same-commit'
        with patch.object(release, 'git', side_effect=git), patch.object(release, 'commits_since', return_value=[]), patch.object(release.subprocess, 'run'):
            self.assertEqual(release.prepare(self.root, 'auto', self.date)[0], tag)
    def test_minor_bump_updates_both_locks_and_preserves_history(self):
        with patch.object(release, 'git', return_value=''):
            release.prepare(self.root, 'auto', self.date)
        def git(*args): return 'v0.1.0' if args[0] == 'tag' else ''
        with patch.object(release, 'git', side_effect=git), patch.object(release, 'commits_since', return_value=[('abcdef', 'feat: interactive UI')]), patch.object(release.subprocess, 'run'):
            tag, _ = release.prepare(self.root, 'auto', self.date)
        self.assertEqual(tag, 'v0.2.0')
        for name in ['Cargo.lock', 'examples/sdk-consumer/Cargo.lock']:
            self.assertIn('name = "werk-worker"\nversion = "0.2.0"', (self.root / name).read_text())
        self.assertIn('## [0.1.0]', (self.root / 'CHANGELOG.md').read_text())
        self.assertIn('Current release: `v0.2.0`', (self.root / 'docs/releases.md').read_text())
    def test_bad_lock_does_not_modify_any_metadata(self):
        p = self.root / 'Cargo.lock'; p.write_text(p.read_text().replace('name = "werk-worker"\nversion = "0.1.0"','name = "werk-worker"\nversion = "0.9.0"'))
        before = (self.root / 'Cargo.toml').read_bytes()
        with patch.object(release, 'git', return_value=''), self.assertRaises(ValueError):
            release.prepare(self.root, 'patch', self.date)
        self.assertEqual((self.root / 'Cargo.toml').read_bytes(), before)
    def test_foreign_existing_tag_cannot_be_reused(self):
        with patch.object(release, 'git', return_value='v0.1.1'), patch.object(release.subprocess, 'run'), self.assertRaises(ValueError):
            release.prepare(self.root, 'patch', self.date)
    def test_commit_inference_and_escaping(self):
        self.assertEqual(release.inferred_bump([('x', 'fix!: new contract')]), 'major')
        self.assertEqual(release.inferred_bump([('x', 'feat(ui): panel')]), 'minor')
        self.assertNotIn('<script>', release.commit_notes([('123456789abcdef','fix: <script> `$HOME`')]))

if __name__ == '__main__': unittest.main()
