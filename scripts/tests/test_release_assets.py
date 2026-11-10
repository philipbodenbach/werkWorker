"""Release asset validation/recovery tests; GitHub is an in-memory double."""

import hashlib
import importlib.util
import io
import json
from pathlib import Path
import stat
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("release_assets", ROOT / "scripts/upload-release-assets.py")
assets = importlib.util.module_from_spec(spec)
spec.loader.exec_module(assets)
TAG = "v0.3.1"
COMMIT = "a" * 40
REPO = "owner/worker"


def digest(data):
    return hashlib.sha256(data).hexdigest()


def archive_bytes(windows=False, marker=b"fixture", extra=None, symlink=False, version="0.3.1"):
    output = io.BytesIO()
    names = ["werk-worker.exe", "ww.exe"] if windows else ["werk-worker", "ww"]
    names += ["README.md", "LICENSE", "NOTICE"]
    if extra:
        names.append(extra)
    readme = f"WerkWorker SDK and both CLI commands share release version **{version}**.\n".encode()
    if windows:
        with zipfile.ZipFile(output, "w") as archive:
            for name in names:
                member = zipfile.ZipInfo(name)
                member.external_attr = (stat.S_IFLNK if symlink and name == "ww.exe" else stat.S_IFREG) << 16
                archive.writestr(member, readme if name == "README.md" else marker)
    else:
        with tarfile.open(fileobj=output, mode="w:gz") as archive:
            for name in names:
                member = tarfile.TarInfo(name)
                content = readme if name == "README.md" else marker
                member.size = len(content)
                if symlink and name == "ww":
                    member.type = tarfile.SYMTYPE
                    member.linkname = "/outside"
                    member.size = 0
                archive.addfile(member, io.BytesIO(content) if member.isfile() else None)
    return output.getvalue()


class FakeGitHub:
    def __init__(self):
        self.files = {}
        self.calls = []
        self.draft = True
        self.commit = COMMIT
        self.fail_upload = None
        self.with_digest = True
        self.bad_digest = None
        self.missing_after_upload = None
        self.change_tag_after_upload = False

    def tag_commit(self, tag):
        self.calls.append(("tag", tag))
        return self.commit

    def release(self, tag):
        self.calls.append(("release", tag))
        return dict(id=12, tag_name=tag, draft=self.draft)

    def assets(self, release_id):
        self.calls.append(("assets", release_id))
        result = []
        for name, data in self.files.items():
            if name == self.missing_after_upload:
                continue
            item = dict(id=name, name=name, state="uploaded")
            if self.with_digest:
                item["digest"] = "sha256:" + ("0" * 64 if name == self.bad_digest else digest(data))
            result.append(item)
        return result

    def download(self, asset, path):
        self.calls.append(("download", asset["name"]))
        path.write_bytes(self.files[asset["id"]])

    def upload(self, tag, path):
        self.calls.append(("upload", path.name))
        if path.name == self.fail_upload:
            raise subprocess.CalledProcessError(1, ["mock-gh-upload"])
        if path.name in self.files:
            raise AssertionError("Attempted to overwrite existing asset")
        self.files[path.name] = path.read_bytes()
        if self.change_tag_after_upload:
            self.commit = "b" * 40

    def publish(self, tag):
        self.calls.append(("publish", tag))
        self.draft = False


class ReleaseAssetTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.client = FakeGitHub()
        self.names = [f"werkWorker-{TAG}-{suffix}" for suffix in assets.SUFFIXES]
        for name in self.names:
            self.write_archive(name)

    def write_archive(self, name, **options):
        data = archive_bytes(windows=name.endswith(".zip"), **options)
        (self.root / name).write_bytes(data)
        (self.root / (name + ".sha256")).write_text(f"{digest(data)}  {name}\n", encoding="ascii")
        return data

    def run_upload(self, draft=True):
        with patch("builtins.print"):
            assets.upload_release(REPO, TAG, COMMIT, self.root, draft, client=self.client)

    def mutations(self):
        return [call for call in self.client.calls if call[0] in ("upload", "publish")]

    def seed_pair(self, name, marker=b"remote build"):
        data = archive_bytes(windows=name.endswith(".zip"), marker=marker)
        self.client.files[name] = data
        self.client.files[name + ".sha256"] = f"{digest(data)}  {name}\n".encode()

    def test_uploads_exact_three_pairs_and_publishes_only_after_verification(self):
        self.run_upload(draft=False)
        self.assertEqual(set(self.client.files), {p.name for p in self.root.iterdir()})
        expected = [("upload", name) for archive in self.names
                    for name in (archive, archive + ".sha256")]
        self.assertEqual(self.mutations(), expected + [("publish", TAG)])
        self.assertEqual(self.client.calls[-2:], [("tag", TAG), ("publish", TAG)])

    def test_default_draft_remains_draft(self):
        self.run_upload()
        self.assertTrue(self.client.draft)
        self.assertNotIn(("publish", TAG), self.client.calls)

    def test_missing_file_wrong_version_and_extra_file_fail_before_github(self):
        for mode in ("missing", "version", "extra"):
            with self.subTest(mode=mode):
                original = self.root / self.names[0]
                renamed = self.root / self.names[0].replace(TAG, "v0.3.0")
                extra = self.root / "extra.txt"
                data = original.read_bytes()
                if mode == "missing":
                    original.unlink()
                elif mode == "version":
                    original.rename(renamed)
                else:
                    extra.write_text("unexpected")
                with self.assertRaisesRegex(ValueError, "exactly three"):
                    self.run_upload()
                self.assertEqual(self.client.calls, [])
                original.write_bytes(data)
                renamed.unlink(missing_ok=True)
                extra.unlink(missing_ok=True)

    def test_corrupt_checksum_and_wrong_checksum_filename_fail_before_github(self):
        name = self.names[0]
        for content in ("0" * 64 + f"  {name}\n", digest((self.root / name).read_bytes()) + "  other.tar.gz\n"):
            with self.subTest(content=content):
                (self.root / (name + ".sha256")).write_text(content)
                with self.assertRaisesRegex(ValueError, "checksum"):
                    self.run_upload()
                self.assertEqual(self.client.calls, [])

    def test_correct_filename_with_wrong_readme_version_fails_before_github(self):
        for name in self.names:
            with self.subTest(name=name):
                self.write_archive(name, version="0.2.0")
                with self.assertRaisesRegex(ValueError, "README release version"):
                    self.run_upload()
                self.assertEqual(self.client.calls, [])
                self.write_archive(name)

    def test_unsafe_members_and_symlinks_are_rejected(self):
        for name in (self.names[0], self.names[2]):
            for options in (dict(extra="../escape"), dict(extra="ww"), dict(symlink=True)):
                with self.subTest(name=name, options=options):
                    self.write_archive(name, **options)
                    with self.assertRaisesRegex(ValueError, "members"):
                        self.run_upload()
                    self.assertEqual(self.client.calls, [])
                    self.write_archive(name)

    def test_invalid_archive_is_rejected_even_with_matching_checksum(self):
        name = self.names[0]
        (self.root / name).write_bytes(b"not a tar file")
        (self.root / (name + ".sha256")).write_text(f"{digest(b'not a tar file')}  {name}\n")
        with self.assertRaises(tarfile.TarError):
            self.run_upload()
        self.assertEqual(self.client.calls, [])

    def test_truncated_gzip_trailer_is_rejected_before_github(self):
        name = self.names[0]
        data = (self.root / name).read_bytes()[:-6]
        (self.root / name).write_bytes(data)
        (self.root / (name + ".sha256")).write_text(f"{digest(data)}  {name}\n")
        with self.assertRaises(EOFError):
            self.run_upload()
        self.assertEqual(self.client.calls, [])

    def test_valid_remote_pairs_win_over_different_rebuilt_bytes(self):
        for name in self.names:
            self.seed_pair(name)
        self.client.files["user-notes.txt"] = b"preserved"
        before = dict(self.client.files)
        self.run_upload()
        self.run_upload()
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.client.files, before)

    def test_existing_archive_without_checksum_is_repaired_from_remote_bytes(self):
        name = self.names[0]
        self.client.files[name] = archive_bytes(marker=b"older valid build")
        self.run_upload()
        self.assertNotIn(("upload", name), self.client.calls)
        expected = f"{digest(self.client.files[name])}  {name}\n".encode()
        self.assertEqual(self.client.files[name + ".sha256"], expected)

    def test_repaired_checksums_use_lf_even_with_windows_text_defaults(self):
        for name in self.names:
            self.client.files[name] = archive_bytes(windows=name.endswith(".zip"),
                                                   marker=b"older valid build")
        original_open = Path.open

        def windows_text_open(path, mode="r", *args, **kwargs):
            # Exercise Windows newline translation on Linux/macOS CI as well.
            if "b" not in mode and any(flag in mode for flag in "wax"):
                if kwargs.get("newline") is None:
                    kwargs["newline"] = "\r\n"
            return original_open(path, mode, *args, **kwargs)

        with patch.object(Path, "open", windows_text_open):
            self.run_upload()
        for name in self.names:
            expected = f"{digest(self.client.files[name])}  {name}\n".encode("ascii")
            self.assertEqual(self.client.files[name + ".sha256"], expected)
            self.assertNotIn(("upload", name), self.client.calls)

    def test_existing_crlf_checksums_are_preserved(self):
        for name in self.names:
            self.seed_pair(name)
            self.client.files[name + ".sha256"] = (
                f"{digest(self.client.files[name])}  {name}\r\n".encode("ascii")
            )
        before = dict(self.client.files)
        self.run_upload()
        self.assertEqual(self.mutations(), [])
        self.assertEqual(self.client.files, before)

    def test_existing_checksum_without_archive_accepts_only_matching_local_bytes(self):
        name = self.names[0]
        self.client.files[name + ".sha256"] = (self.root / (name + ".sha256")).read_bytes()
        self.run_upload()
        self.assertIn(("upload", name), self.client.calls)
        self.assertNotIn(("upload", name + ".sha256"), self.client.calls)

    def test_conflicting_or_invalid_remote_pairs_block_all_uploads(self):
        # Last platform fails: earlier missing platforms must not yet be uploaded.
        name = self.names[-1]
        for remote_archive in (False, True):
            with self.subTest(remote_archive=remote_archive):
                self.client = FakeGitHub()
                if remote_archive:
                    self.client.files[name] = archive_bytes(windows=True)
                self.client.files[name + ".sha256"] = ("0" * 64 + f"  {name}\n").encode()
                with self.assertRaisesRegex(ValueError, "Existing checksum"):
                    self.run_upload(draft=False)
                self.assertEqual(self.mutations(), [])

    def test_bad_remote_archive_blocks_uploads_even_with_its_matching_checksum(self):
        name = self.names[-1]
        data = archive_bytes(windows=True, symlink=True)
        self.client.files[name] = data
        self.client.files[name + ".sha256"] = f"{digest(data)}  {name}\n".encode()
        with self.assertRaisesRegex(ValueError, "members"):
            self.run_upload()
        self.assertEqual(self.mutations(), [])

    def test_existing_remote_digest_mismatch_blocks_all_uploads(self):
        name = self.names[-1]
        self.seed_pair(name)
        self.client.bad_digest = name
        with self.assertRaisesRegex(ValueError, "digest mismatch"):
            self.run_upload(draft=False)
        self.assertEqual(self.mutations(), [])

    def test_remote_archive_with_wrong_readme_version_blocks_all_uploads(self):
        name = self.names[-1]
        data = archive_bytes(windows=True, version="0.2.0")
        self.client.files[name] = data
        self.client.files[name + ".sha256"] = f"{digest(data)}  {name}\n".encode()
        with self.assertRaisesRegex(ValueError, "README release version"):
            self.run_upload(draft=False)
        self.assertEqual(self.mutations(), [])

    def test_failed_checksum_upload_can_resume_without_overwriting_archive(self):
        name = self.names[0]
        self.client.fail_upload = name + ".sha256"
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_upload(draft=False)
        self.assertTrue(self.client.draft)
        original = self.client.files[name]
        self.client.calls.clear()
        self.client.fail_upload = None
        self.write_archive(name, marker=b"later rebuild")
        self.run_upload(draft=False)
        self.assertEqual(self.client.files[name], original)
        self.assertNotIn(("upload", name), self.client.calls)
        self.assertEqual(self.client.calls[-1], ("publish", TAG))

    def test_wrong_tag_sha_blocks_uploads(self):
        self.client.commit = "b" * 40
        with self.assertRaisesRegex(ValueError, "expected commit"):
            self.run_upload(draft=False)
        self.assertEqual(self.mutations(), [])

    def test_tag_changed_during_upload_prevents_publication(self):
        self.client.change_tag_after_upload = True
        with self.assertRaisesRegex(ValueError, "expected commit"):
            self.run_upload(draft=False)
        self.assertTrue(self.client.draft)
        self.assertNotIn(("publish", TAG), self.client.calls)

    def test_final_missing_asset_or_bad_digest_prevents_publication(self):
        for problem in ("missing_after_upload", "bad_digest"):
            with self.subTest(problem=problem):
                self.client = FakeGitHub()
                setattr(self.client, problem, self.names[-1])
                with self.assertRaisesRegex(ValueError, "missing|digest mismatch"):
                    self.run_upload(draft=False)
                self.assertTrue(self.client.draft)
                self.assertNotIn(("publish", TAG), self.client.calls)

    def test_digest_fallback_downloads_and_verifies_remote_bytes(self):
        self.client.with_digest = False
        self.run_upload(draft=False)
        downloads = [call for call in self.client.calls if call[0] == "download"]
        self.assertEqual(len(downloads), 6)
        self.assertFalse(self.client.draft)

    def test_published_release_is_not_demoted_or_republished(self):
        for draft in (True, False):
            with self.subTest(draft=draft):
                self.client = FakeGitHub()
                self.client.draft = False
                for name in self.names:
                    self.seed_pair(name)
                self.run_upload(draft=draft)
                self.assertEqual(self.mutations(), [])
                self.assertFalse(self.client.draft)


class GitHubClientTests(unittest.TestCase):
    def test_resolves_draft_and_published_releases_by_id(self):
        client = assets.GitHub(REPO)
        for draft in (True, False):
            with self.subTest(draft=draft):
                release = dict(id=12, tag_name=TAG, draft=draft)

                def github(command, **kwargs):
                    if command == ["gh", "release", "view", TAG, "--repo", REPO,
                                   "--json", "apiUrl"]:
                        reply = dict(apiUrl=f"https://api.github.com/repos/{REPO}/releases/12")
                    elif command == ["gh", "api", f"repos/{REPO}/releases/12"]:
                        reply = release
                    elif command == ["gh", "api", f"repos/{REPO}/releases/tags/{TAG}"]:
                        if draft:
                            raise subprocess.CalledProcessError(
                                1, command, stderr="gh: Not Found (HTTP 404)\n")
                        reply = release
                    else:
                        self.fail(f"Unexpected GitHub command: {command}")
                    return subprocess.CompletedProcess(command, 0, json.dumps(reply), "")

                with patch.object(assets.subprocess, "run", side_effect=github):
                    self.assertEqual(client.release(TAG), release)

    def test_release_tag_mismatch_is_rejected(self):
        client = assets.GitHub(REPO)
        replies = [dict(apiUrl=f"https://api.github.com/repos/{REPO}/releases/12"),
                   dict(id=12, tag_name="v9.9.9", draft=True)]
        with patch.object(assets.subprocess, "run", side_effect=[
                subprocess.CompletedProcess([], 0, json.dumps(reply), "") for reply in replies]):
            with self.assertRaisesRegex(ValueError, "Release tag does not match"):
                client.release(TAG)

    def test_failed_release_lookup_stops_before_fetching_assets(self):
        client = assets.GitHub(REPO)
        error = subprocess.CalledProcessError(1, ["gh"], stderr="release not found\n")
        with patch.object(assets.subprocess, "run", side_effect=error) as run:
            with self.assertRaises(subprocess.CalledProcessError):
                client.release(TAG)
            self.assertEqual(run.call_count, 1)

    def test_resolves_annotated_and_lightweight_tags(self):
        client = assets.GitHub(REPO)
        for replies in ([{"object": {"type": "commit", "sha": COMMIT}}],
                        [{"object": {"type": "tag", "sha": "b" * 40}},
                         {"object": {"type": "commit", "sha": COMMIT}}]):
            with self.subTest(replies=replies), patch.object(client, "api", side_effect=replies):
                self.assertEqual(client.tag_commit(TAG), COMMIT)

    def test_non_commit_and_cyclic_tags_are_rejected(self):
        client = assets.GitHub(REPO)
        for kind in ("tree", "tag"):
            with self.subTest(kind=kind), patch.object(client, "api", return_value={"object": {"type": kind, "sha": COMMIT}}):
                with self.assertRaises(ValueError):
                    client.tag_commit(TAG)

    def test_upload_command_never_overwrites_and_publish_only_changes_draft(self):
        client = assets.GitHub(REPO)
        with patch.object(assets.subprocess, "run") as run:
            client.upload(TAG, Path("archive.tar.gz"))
            self.assertEqual(run.call_args.args[0],
                             ["gh", "release", "upload", TAG, "archive.tar.gz", "--repo", REPO])
            client.publish(TAG)
            self.assertEqual(run.call_args.args[0],
                             ["gh", "release", "edit", TAG, "--draft=false", "--repo", REPO])

    def test_asset_listing_paginates_without_losing_later_platforms(self):
        client = assets.GitHub(REPO)
        with patch.object(client, "api", side_effect=[[dict(name=str(i)) for i in range(100)], [dict(name="last")]]) as api:
            self.assertEqual(len(client.assets(12)), 101)
            self.assertEqual(api.call_args.args[0], "releases/12/assets?per_page=100&page=2")


class CommandLineTests(unittest.TestCase):
    def test_github_failure_reports_captured_diagnostics_and_exits_unsuccessfully(self):
        command = ["gh", "api", f"repos/{REPO}/releases/12"]
        argv = ["upload-release-assets.py", "--repo", REPO, "--tag", TAG,
                "--commit", COMMIT, "--directory", "unused", "--draft", "false"]
        for diagnostic in ("gh: Not Found (HTTP 404)\n",
                           "gh: Resource not accessible by integration (HTTP 403)\n", None):
            with self.subTest(diagnostic=diagnostic):
                error = subprocess.CalledProcessError(1, command, stderr=diagnostic)
                output = io.StringIO()
                with patch.object(assets.sys, "argv", argv), \
                        patch.object(assets.sys, "stderr", output), \
                        patch.object(assets, "upload_release", side_effect=error):
                    self.assertEqual(assets.main(), 1)
                self.assertIn("Release asset upload failed:", output.getvalue())
                if diagnostic:
                    self.assertIn(diagnostic, output.getvalue())


if __name__ == "__main__":
    unittest.main()
