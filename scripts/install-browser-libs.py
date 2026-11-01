"""Optional Debian/Ubuntu library bundle; never installs packages system-wide.

Use through install-browser.sh --local-deps. Package names come from the pinned
Playwright install-deps plan; APT resolves/downloads signed repository packages.
Only .deb payloads are extracted: no package scripts or sudo are executed.
"""
import json
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile


def main():
    try:
        import apt
    except ImportError:
        raise SystemExit("--local-deps requires Debian/Ubuntu system Python with python3-apt")
    root = Path(sys.argv[1]).resolve() / "deps"
    engines = sys.argv[2:]
    plan = subprocess.check_output(
        [sys.executable, "-m", "playwright", "install-deps", "--dry-run", *engines], text=True)
    command = next((line for line in plan.splitlines() if "apt-get install" in line), None)
    if not command:
        raise SystemExit("Playwright did not provide a supported APT dependency plan")
    outer = shlex.split(command)
    inner = shlex.split(outer[-1]) if "-c" in outer else outer
    try:
        names = inner[inner.index("--no-install-recommends") + 1:]
    except ValueError:
        raise SystemExit("Unrecognized Playwright dependency plan; no changes made")
    if not names or any(not re.fullmatch(r"[a-z0-9][a-z0-9+.:_-]*", name) for name in names):
        raise SystemExit("Invalid package names in dependency plan")
    cache = apt.Cache()
    needed = {}

    def visit(name):
        package = cache[name]
        if package.is_installed or name in needed:
            return
        version = package.candidate
        if version is None:
            raise SystemExit(f"No repository candidate for {name}")
        needed[name] = version
        for group in version.dependencies:
            if group.rawtype not in ("Depends", "PreDepends"):
                continue
            choices = [v for dep in group.or_dependencies for v in dep.target_versions]
            if any(v.is_installed for v in choices):
                continue
            chosen = next((v for v in choices if v == v.package.candidate), None)
            if chosen is None:
                raise SystemExit(f"Unresolved dependency: {group}")
            visit(chosen.package.name)

    for name in names:
        visit(name)
    root.mkdir(parents=True, exist_ok=True)
    if needed:
        print(f"Downloading {len(needed)} missing packages into {root}")
        with tempfile.TemporaryDirectory(prefix="werk-browser-libs-") as download:
            subprocess.run(["apt-get", "download", *[f"{name}={v.version}" for name, v in sorted(needed.items())]], cwd=download, check=True)
            for package in sorted(Path(download).glob("*.deb")):
                subprocess.run(["dpkg-deb", "--extract", str(package), str(root)], check=True)
        (root / "manifest.json").write_text(json.dumps(
            {name: version.version for name, version in sorted(needed.items())}, indent=2) + "\n")
    print(f"Local browser libraries ready: {root}")


if __name__ == "__main__":
    main()
