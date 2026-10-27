#!/usr/bin/env python3
"""Prepare release metadata automatically, or validate it before publication."""

import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib
from zoneinfo import ZoneInfo


VERSION = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
DOC_VERSION = r"(?P<version>[0-9]+\.[0-9]+\.[0-9]+)"
# Explicit current-release fields: do not rewrite historical validation results,
# dependency versions, protocol versions, or arbitrary prose throughout docs/.
DOC_FIELDS = {
    "docs/releases.md": [r"Current release: `v" + DOC_VERSION],
}



def update_doc_fields(files, current, next_version):
    for path, patterns in DOC_FIELDS.items():
        for pattern in patterns:
            expected = current
            replacement = next_version
            matches = list(re.finditer(pattern, files[path]))
            if not matches or any(match["version"] != expected for match in matches):
                raise ValueError(f"Missing or stale release version in {path}: expected {expected}")

            def replace(match):
                start, end = match.span("version")
                return match[0][:start - match.start()] + replacement + match[0][end - match.start():]

            files[path] = re.sub(pattern, replace, files[path])


def readme_section(text, version):
    pattern = rf"^## What’s new in v{re.escape(version)}\n.*?(?=^## |\Z)"
    matches = list(re.finditer(pattern, text, re.M | re.S))
    if len(matches) != 1:
        raise ValueError(f"README.md must contain one What's new section for v{version}")
    return matches[0]


def update_readme(text, current, next_version, date, notes):
    highlights = re.search(r'^### Highlights\n(.*?)(?=^### |\Z)', notes, re.M | re.S)
    if highlights and re.search(r'^- \S', highlights[1], re.M):
        summary = highlights[1].strip()
    else:
        # Preserve multiline bullets; a summary is copied, never invented.
        bullets = re.findall(r'^- \S[^\n]*(?:\n[ \t]+[^\n]+)*', notes, re.M)
        summary = "\n".join(bullets[:5])
        if not summary:
            raise ValueError("Release notes must contain at least one bullet for the README")
    section = readme_section(text, current)
    following = text[section.end():]
    anchor = next_version.replace(".", "") + "---" + str(date)
    new_section = (
        f"## What’s new in v{next_version}\n\n"
        f"WerkWorker SDK and both CLI commands share release version **{next_version}**.\n"
        "\n"
        f"{summary}\n\n"
        f"See the [v{next_version} changelog](CHANGELOG.md#{anchor}) "
        "for all changes and compatibility notes.\n"
    )
    # Separate a following section, but never add a blank line at EOF.
    return text[:section.start()] + new_section + ("\n" if following else "") + following


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def replace_once(text, pattern, replacement):
    result, count = re.subn(pattern, lambda match: replacement(match), text, flags=re.M)
    if count != 1:
        raise ValueError(f"Expected exactly one match for {pattern!r}, found {count}")
    return result


def commits_since(tag):
    revisions = git("rev-list", "--reverse", "--no-merges", f"{tag}..HEAD").splitlines()
    return [(revision, git("show", "-s", "--format=%B", revision)) for revision in revisions]


def inferred_bump(commits):
    for _, message in commits:
        if re.match(r"^[a-zA-Z][\w-]*(?:\([^\n)]*\))?!:", message) or re.search(
            r"^BREAKING[ -]CHANGE:", message, re.M
        ):
            return "major"
    if any(re.match(r"^feat(?:\([^\n)]*\))?:", message) for _, message in commits):
        return "minor"
    return "patch"


def commit_notes(commits):
    # Escape Markdown/HTML syntax in commit subjects; never interpolate into a shell.
    bullets = []
    for revision, message in commits:
        subject = message.splitlines()[0]
        subject = subject.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
        subject = re.sub(r"([\\`*_\[\]])", r"\\\1", subject)
        bullets.append(f"- {subject} (`{revision[:12]}`)")
    return "### Changes\n\n" + "\n".join(bullets) + "\n"


def prepare(root, bump, date, allow_existing=False):
    paths = ['Cargo.toml', 'Cargo.lock', 'examples/sdk-consumer/Cargo.lock',
             'CHANGELOG.md', 'README.md', *DOC_FIELDS]
    files = {p: (root / p).read_text() for p in paths}
    current = tomllib.loads(files['Cargo.toml'])['package']['version']
    if not re.fullmatch(VERSION, current):
        raise ValueError('Expected a stable SemVer package version')
    for path in ['Cargo.lock', 'examples/sdk-consumer/Cargo.lock']:
        locked = next(p['version'] for p in tomllib.loads(files[path])['package']
                      if p['name'] == 'werk-worker' and 'source' not in p)
        if locked != current:
            raise ValueError(f'{path} version differs from Cargo.toml')
    tags = [t for t in git('tag', '--list').splitlines() if re.fullmatch('v' + VERSION, t)]
    key = lambda v: tuple(map(int, v.lstrip('v').split('.')))
    latest = max(tags, key=key) if tags else None
    if latest:
        subprocess.run(['git', 'merge-base', '--is-ancestor', latest, 'HEAD'], check=True)
    # Resume an already prepared release before applying any new increment.
    # A workflow retry keeps its original input (e.g. minor) after the PR merged.
    if bump != 'check':
        if not latest:
            if re.search(r'^## \[' + re.escape(current) + r'\] - ', files['CHANGELOG.md'], re.M):
                return prepare(root, 'check', date)
            if bump == 'auto':
                bump = 'initial'
        elif key(current) > key(latest):
            return prepare(root, 'check', date)
        elif latest != 'v' + current:
            raise ValueError('Latest stable tag differs from package version')
        else:
            commits = commits_since(latest)
            if not commits:
                return prepare(root, 'check', date, allow_existing=True)
            if bump == 'auto':
                bump = inferred_bump(commits)
    major, minor, patch = key(current)
    version = {'initial': current, 'check': current, 'patch': f'{major}.{minor}.{patch+1}',
               'minor': f'{major}.{minor+1}.0', 'major': f'{major+1}.0.0'}[bump]
    tag = 'v' + version
    if tag in tags:
        if not allow_existing or git('rev-parse', tag + '^{commit}') != git('rev-parse', 'HEAD'):
            raise ValueError(f'Tag {tag} already exists')
        older = [t for t in tags if t != tag]
        latest = max(older, key=key) if older else None
    if bump == 'check':
        if latest and key(version) <= key(latest):
            raise ValueError('Prepared version must be newer than the previous release')
        sections = list(re.finditer(r'^## \[([^\]]+)\]([^\n]*)$', files['CHANGELOG.md'], re.M))
        if len(sections) < 2 or sections[0][1] != 'Unreleased' or sections[1][1] != current:
            raise ValueError('First dated changelog section must match the package')
        if sum(s[1] == current for s in sections) != 1:
            raise ValueError('Duplicate release section')
        datetime.strptime(sections[1][2], ' - %Y-%m-%d')
        end = sections[2].start() if len(sections) > 2 else len(files['CHANGELOG.md'])
        notes = files['CHANGELOG.md'][sections[1].end():end].split('\n[Unreleased]:')[0].strip()
        if not re.search(r'^- \S', notes, re.M):
            raise ValueError('Release notes must contain a bullet')
        update_doc_fields(files, current, current)
        readme_section(files['README.md'], current)
        if f'release version **{current}**' not in files['README.md']:
            raise ValueError('README version differs')
        return tag, notes + '\n'
    if latest and latest != 'v' + current:
        raise ValueError('Latest tag differs from current package')
    files['Cargo.toml'] = replace_once(files['Cargo.toml'],
        r'(\[package\]\n(?:(?!\[)[^\n]*\n)*?version = ")[^"]+("[^\n]*$)',
        lambda m: m[1] + version + m[2])
    for path in ['Cargo.lock', 'examples/sdk-consumer/Cargo.lock']:
        files[path] = replace_once(files[path],
            r'(\[\[package\]\]\nname = "werk-worker"\nversion = ")[^"]+("$)',
            lambda m: m[1] + version + m[2])
    text = files['CHANGELOG.md']
    unreleased = re.search(r'^## \[Unreleased\]\n(.*?)(?=^## \[|^\[Unreleased\]:|\Z)', text, re.M | re.S)
    if not unreleased:
        raise ValueError('Missing Unreleased section')
    notes = unreleased[1].strip()
    if not re.search(r'^- \S', notes, re.M):
        commits = commits_since(latest) if latest else []
        if not commits:
            raise ValueError('No release changes')
        notes = commit_notes(commits).strip()
    files['CHANGELOG.md'] = text[:unreleased.start()] + f'## [Unreleased]\n\n## [{version}] - {date}\n\n{notes}\n\n' + text[unreleased.end():]
    files['CHANGELOG.md'] = re.sub(r'^\[Unreleased\]:[^\n]*\n?', '', files['CHANGELOG.md'], flags=re.M)
    files['CHANGELOG.md'] += f'\n[Unreleased]: https://github.com/philipbodenbach/werkWorker/compare/{tag}...HEAD\n'
    if latest:
        files['CHANGELOG.md'] += f'[{version}]: https://github.com/philipbodenbach/werkWorker/compare/{latest}...{tag}\n'
    update_doc_fields(files, current, version)
    files['README.md'] = update_readme(files['README.md'], current, version, date, notes)
    for path, contents in files.items():
        (root / path).write_text(contents)
    return tag, notes + '\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('bump', choices=['auto', 'patch', 'minor', 'major', 'check'])
    parser.add_argument('--notes-file', type=Path, required=True)
    args = parser.parse_args()
    if args.bump != 'check' and git('status', '--porcelain', '--untracked-files=no'):
        parser.error('Release preparation requires a clean tracked working tree')
    try:
        tag, notes = prepare(Path.cwd(), args.bump, datetime.now(ZoneInfo('Europe/Berlin')).date())
    except (OSError, ValueError, KeyError, StopIteration, subprocess.CalledProcessError) as error:
        parser.exit(1, f'Release preparation failed: {error}\n')
    args.notes_file.write_text(notes)
    if output := os.environ.get('GITHUB_OUTPUT'):
        with open(output, 'a') as stream:
            stream.write(f'tag={tag}\n')
    print(tag)


if __name__ == '__main__':
    main()
