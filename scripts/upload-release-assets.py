#!/usr/bin/env python3
"""Attach validated native packages to an existing release without replacing assets."""

import argparse
import gzip
import hashlib
import json
from pathlib import Path
import re
import stat
import subprocess
import sys
import tarfile
import tempfile
import zipfile


SUFFIXES = ("linux-x86_64.tar.gz", "macos-aarch64.tar.gz", "windows-x86_64.zip")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def checksum(path, archive_name):
    if path.stat().st_size > 1024:
        raise ValueError(f"Invalid checksum file: {path.name}")
    match = re.fullmatch(r"([0-9a-fA-F]{64}) [ *]" + re.escape(archive_name) + r"\r?\n?",
                         path.read_text(encoding="ascii"))
    if not match:
        raise ValueError(f"Invalid checksum or archive name: {path.name}")
    return match[1].lower()


def validate_archive(path):
    version = re.match(r"werkWorker-v([0-9]+\.[0-9]+\.[0-9]+)-", path.name)
    if version is None:
        raise ValueError(f"Missing archive version: {path.name}")
    windows = path.name.endswith(".zip")
    binaries = {"werk-worker.exe", "ww.exe"} if windows else {"werk-worker", "ww"}
    expected = binaries | {"README.md", "LICENSE", "NOTICE"}
    readme = b""
    if windows:
        with zipfile.ZipFile(path) as archive:
            members = archive.infolist()
            names = [member.filename for member in members]
            regular = all(not member.is_dir() and
                          not member.flag_bits & 1 and
                          stat.S_IFMT(member.external_attr >> 16) in (0, stat.S_IFREG)
                          for member in members)
            if regular and archive.testzip() is not None:
                raise ValueError(f"Corrupt ZIP contents: {path.name}")
            if len(names) == 5 and set(names) == expected and regular:
                if archive.getinfo("README.md").file_size > 1024 * 1024:
                    raise ValueError(f"Oversized release README: {path.name}")
                readme = archive.read("README.md")
    else:
        with tarfile.open(path, "r:gz") as archive:
            members = archive.getmembers()
            names = [member.name for member in members]
            regular = all(member.isfile() for member in members)
            if len(names) == 5 and set(names) == expected and regular:
                if archive.getmember("README.md").size > 1024 * 1024:
                    raise ValueError(f"Oversized release README: {path.name}")
                with archive.extractfile("README.md") as source:
                    readme = source.read()
        # Tar readers can stop at the end marker before reaching the gzip CRC.
        # Drain the compressed stream to reject a truncated/corrupt trailer too.
        with gzip.open(path, "rb") as compressed:
            for _ in iter(lambda: compressed.read(1024 * 1024), b""):
                pass
    if len(names) != 5 or set(names) != expected or not regular:
        raise ValueError(f"Unexpected members or non-regular files in {path.name}")
    versions = re.findall(rb"release version \*\*([0-9]+\.[0-9]+\.[0-9]+)\*\*", readme)
    if versions != [version[1].encode("ascii")]:
        raise ValueError(f"README release version does not match archive: {path.name}")


class GitHub:
    def __init__(self, repo):
        self.repo = repo

    def api(self, endpoint):
        result = subprocess.run(["gh", "api", f"repos/{self.repo}/{endpoint}"],
                                check=True, capture_output=True, text=True)
        return json.loads(result.stdout)

    def tag_commit(self, tag):
        obj = self.api(f"git/ref/tags/{tag}")["object"]
        seen = set()
        while obj["type"] == "tag":
            if obj["sha"] in seen or len(seen) >= 10:
                raise ValueError(f"Cyclic or excessive annotated tag chain: {tag}")
            seen.add(obj["sha"])
            obj = self.api(f"git/tags/{obj['sha']}")["object"]
        if obj["type"] != "commit":
            raise ValueError(f"Tag {tag} does not point to a commit")
        return obj["sha"]

    def release(self, tag):
        # The REST tag endpoint only finds published releases. Let gh resolve
        # drafts too, then fetch by database ID to keep the same REST fields.
        result = subprocess.run(["gh", "release", "view", tag, "--repo", self.repo,
                                 "--json", "apiUrl"],
                                check=True, capture_output=True, text=True)
        release_id = json.loads(result.stdout)["apiUrl"].rsplit("/", 1)[-1]
        release = self.api(f"releases/{release_id}")
        if release["tag_name"] != tag:
            raise ValueError("Release tag does not match the requested version")
        return release

    def assets(self, release_id):
        assets = []
        page = 1
        while True:
            batch = self.api(f"releases/{release_id}/assets?per_page=100&page={page}")
            assets.extend(batch)
            if len(batch) < 100:
                return assets
            page += 1

    def download(self, asset, destination):
        with destination.open("wb") as output:
            subprocess.run(["gh", "api", f"repos/{self.repo}/releases/assets/{asset['id']}",
                            "-H", "Accept: application/octet-stream"], check=True, stdout=output)

    def upload(self, tag, path):
        subprocess.run(["gh", "release", "upload", tag, str(path), "--repo", self.repo], check=True)

    def publish(self, tag):
        subprocess.run(["gh", "release", "edit", tag, "--draft=false", "--repo", self.repo], check=True)


def require_tag(client, tag, commit):
    if client.tag_commit(tag) != commit:
        raise ValueError(f"Remote tag {tag} no longer points to expected commit {commit}")


def asset_map(client, release_id, expected):
    result = {}
    for asset in client.assets(release_id):
        name = asset["name"]
        if name not in expected:
            continue  # Preserve unrelated existing release assets.
        if name in result or asset.get("state", "uploaded") != "uploaded":
            raise ValueError(f"Duplicate or incomplete remote asset: {name}")
        result[name] = asset
    return result


def download_checked(client, asset, path):
    client.download(asset, path)
    actual = sha256(path)
    if asset.get("digest") is not None and asset["digest"] != "sha256:" + actual:
        raise ValueError(f"Remote asset digest mismatch: {asset['name']}")
    return actual


def upload_release(repo, tag, commit, directory, draft, client=None):
    if not re.fullmatch(r"[\w.-]+/[\w.-]+", repo, re.ASCII):
        raise ValueError("Expected repository owner/name")
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise ValueError("Expected stable version tag vX.Y.Z")
    if not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("Expected a full lowercase commit SHA")
    directory = Path(directory)
    names = [f"werkWorker-{tag}-{suffix}" for suffix in SUFFIXES]
    expected = {name for archive in names for name in (archive, archive + ".sha256")}
    files = list(directory.iterdir())
    if {path.name for path in files} != expected or any(
            path.is_symlink() or not path.is_file() for path in files):
        raise ValueError("Expected exactly three versioned native archives and their checksum files")
    local_hashes = {}
    for name in names:
        path = directory / name
        validate_archive(path)
        local_hashes[name] = sha256(path)
        if checksum(directory / (name + ".sha256"), name) != local_hashes[name]:
            raise ValueError(f"Local checksum mismatch: {name}")

    client = client or GitHub(repo)
    require_tag(client, tag, commit)
    release = client.release(tag)  # Never create a release, tag or PR here.
    existing = asset_map(client, release["id"], expected)
    with tempfile.TemporaryDirectory(prefix="werkWorker-release-assets-") as temporary:
        temporary = Path(temporary)
        uploads = []
        final_hashes = {}
        for name in names:
            checksum_name = name + ".sha256"
            archive_path = directory / name
            archive_hash = local_hashes[name]
            if name in existing:
                archive_path = temporary / name
                archive_hash = download_checked(client, existing[name], archive_path)
                validate_archive(archive_path)
            else:
                uploads.append(directory / name)

            if checksum_name in existing:
                checksum_path = temporary / checksum_name
                download_checked(client, existing[checksum_name], checksum_path)
                if checksum(checksum_path, name) != archive_hash:
                    raise ValueError(f"Existing checksum does not match archive: {name}")
            elif name in existing:
                # Repair a previous upload that stopped before its checksum.
                checksum_path = temporary / checksum_name
                # Fixed bytes keep the checksum file identical on every host.
                checksum_path.write_bytes(f"{archive_hash}  {name}\n".encode("ascii"))
                uploads.append(checksum_path)
            else:
                checksum_path = directory / checksum_name
                uploads.append(checksum_path)
            final_hashes[name] = archive_hash
            final_hashes[checksum_name] = sha256(checksum_path)

        # All local and existing remote pairs passed before the first mutation.
        require_tag(client, tag, commit)
        for path in uploads:
            client.upload(tag, path)

        current = client.release(tag)
        if current["id"] != release["id"]:
            raise ValueError("Release was replaced during upload")
        uploaded = asset_map(client, current["id"], expected)
        if set(uploaded) != expected:
            raise ValueError("Release is missing required native assets after upload")
        for name, expected_hash in final_hashes.items():
            digest = uploaded[name].get("digest")
            if digest is None:
                downloaded = temporary / ("verify-" + name)
                client.download(uploaded[name], downloaded)
                digest = "sha256:" + sha256(downloaded)
            if digest != "sha256:" + expected_hash:
                raise ValueError(f"Remote asset digest mismatch: {name}")
        require_tag(client, tag, commit)
        if not draft and current["draft"]:
            client.publish(tag)
        print(f"Verified three native archives and checksums for {tag}; "
              f"uploaded {len(uploads)} missing assets.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--directory", required=True, type=Path)
    parser.add_argument("--draft", required=True, choices=("true", "false"))
    args = parser.parse_args()
    try:
        upload_release(args.repo, args.tag, args.commit, args.directory, args.draft == "true")
    except (ValueError, OSError, EOFError, subprocess.CalledProcessError, tarfile.TarError,
            zipfile.BadZipFile, UnicodeError) as error:
        print(f"Release asset upload failed: {error}", file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr.rstrip(), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
