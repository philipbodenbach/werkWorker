import datetime
import importlib.util
from pathlib import Path
import subprocess
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
    def test_explicit_bumps_resume_untagged_preparation_without_version_drift(self):
        with patch.object(release, 'git', return_value=''):
            expected = release.prepare(self.root, 'minor', self.date)
        before = {p: p.read_bytes() for p in self.root.rglob('*') if p.is_file()}
        # This is also the state after the preparation PR has merged but tagging failed.
        for tags in ('', 'v0.1.0'):
            for bump in ('auto', 'patch', 'minor', 'major'):
                with self.subTest(tags=tags, bump=bump):
                    with patch.object(release, 'git', return_value=tags), \
                            patch.object(release.subprocess, 'run'), \
                            patch.object(release, 'commits_since', return_value=[('fix-sha', 'fix: release workflow')]):
                        self.assertEqual(release.prepare(
                            self.root, bump, self.date + datetime.timedelta(days=1),
                        ), expected)
                    self.assertEqual({p: p.read_bytes() for p in before}, before)
    def test_explicit_bumps_resume_tagged_release_without_new_commits(self):
        with patch.object(release, 'git', return_value=''):
            expected = release.prepare(self.root, 'minor', self.date)
        before = {p: p.read_bytes() for p in self.root.rglob('*') if p.is_file()}
        def git(*args):
            return 'v0.2.0' if args[0] == 'tag' else 'release-commit'
        for bump in ('auto', 'patch', 'minor', 'major'):
            with self.subTest(bump=bump):
                with patch.object(release, 'git', side_effect=git), \
                        patch.object(release.subprocess, 'run'), \
                        patch.object(release, 'commits_since', return_value=[]):
                    self.assertEqual(release.prepare(self.root, bump, self.date), expected)
                self.assertEqual({p: p.read_bytes() for p in before}, before)
    def test_explicit_bump_still_prepares_new_release_after_published_changes(self):
        with patch.object(release, 'git', return_value=''):
            release.prepare(self.root, 'auto', self.date)
        with patch.object(release, 'git', return_value='v0.1.0'), \
                patch.object(release.subprocess, 'run'), \
                patch.object(release, 'commits_since', return_value=[('fix-sha', 'fix: release workflow')]):
            tag, notes = release.prepare(self.root, 'minor', self.date)
        self.assertEqual(tag, 'v0.2.0')
        self.assertIn('fix: release workflow', notes)
    def test_resuming_prepared_version_still_validates_metadata(self):
        with patch.object(release, 'git', return_value=''):
            release.prepare(self.root, 'minor', self.date)
        p = self.root / 'docs/releases.md'
        p.write_text('Current release: `v0.1.0`.\n')
        before = {p: p.read_bytes() for p in self.root.rglob('*') if p.is_file()}
        with patch.object(release, 'git', return_value=''), \
                self.assertRaisesRegex(ValueError, 'stale release version'):
            release.prepare(self.root, 'minor', self.date)
        self.assertEqual({p: p.read_bytes() for p in before}, before)
    def test_retry_does_not_reuse_existing_tag_at_another_commit(self):
        with patch.object(release, 'git', return_value=''):
            release.prepare(self.root, 'minor', self.date)
        def git(*args):
            if args[0] == 'tag':
                return 'v0.2.0'
            return 'new-head' if args[-1] == 'HEAD' else 'tagged-commit'
        with patch.object(release, 'git', side_effect=git), \
                patch.object(release.subprocess, 'run'), \
                patch.object(release, 'commits_since', return_value=[]), \
                self.assertRaisesRegex(ValueError, 'Tag v0.2.0 already exists'):
            release.prepare(self.root, 'minor', self.date)
    def test_empty_unprepared_release_still_fails_without_writing_metadata(self):
        (self.root / 'CHANGELOG.md').write_text('# Changelog\n\n## [Unreleased]\n')
        before = {p: p.read_bytes() for p in self.root.rglob('*') if p.is_file()}
        with patch.object(release, 'git', return_value=''), \
                self.assertRaisesRegex(ValueError, 'No release changes'):
            release.prepare(self.root, 'minor', self.date)
        self.assertEqual({p: p.read_bytes() for p in before}, before)
    def test_prepared_readme_at_eof_passes_git_whitespace_check(self):
        before = self.root / 'README.before.md'
        before.write_bytes((self.root / 'README.md').read_bytes())
        with patch.object(release, 'git', return_value=''):
            release.prepare(self.root, 'minor', self.date)
        result = subprocess.run(
            ['git', '-c', 'core.whitespace=blank-at-eof', 'diff', '--no-index',
             '--check', '--', str(before), str(self.root / 'README.md')],
            capture_output=True, text=True,
        )
        # --no-index may report a content difference (1) even with no whitespace errors.
        self.assertIn(result.returncode, (0, 1), result.stdout + result.stderr)
        self.assertEqual(result.stdout + result.stderr, '')
    def test_readme_update_preserves_following_section_and_separator(self):
        following = '## Installation\n\nRun `ww` after installing.\n'
        readme = (self.root / 'README.md').read_text() + '\n' + following
        updated = release.update_readme(
            readme, '0.1.0', '0.2.0', self.date, '- Added a feature.\n',
        )
        self.assertTrue(updated.endswith('for all changes and compatibility notes.\n\n' + following))
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
