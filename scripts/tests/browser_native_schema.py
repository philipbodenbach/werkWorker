#!/usr/bin/env python3
"""Test actual WW schemas against an installed llama.cpp, without inference.

Requires that runtime's llama.cpp sources and build/bin shared libraries.
Reads only /props from the given local server to obtain its current template.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile
import urllib.request

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("binary", type=Path)
parser.add_argument("--runtime", required=True, type=Path)
parser.add_argument("--server", required=True, help="Local llama-server URL")
args = parser.parse_args()
tools = {t["name"]: t for t in json.loads(subprocess.check_output([str(args.binary.resolve()), "tools", "list"]))}
with urllib.request.urlopen(args.server.rstrip("/") + "/props", timeout=10) as response:
    template = json.load(response)["chat_template"]

cases = []
for label, name, arguments in [
    ("open URL with optional controls", "browser.open", {"url": "https://example.com", "browser": "chromium", "allowed_origins": ["https://example.com"], "private_origins": [], "screenshot": True, "visual": True}),
    ("open workspace file", "browser.open", {"path": "index.html", "screenshot": False}),
    ("mouse click", "browser.action", {"browser_id": "browser_fixture", "action": "click_at", "x": 100, "y": 150, "button": "right", "click_count": 2}),
    ("mouse drag", "browser.action", {"browser_id": "browser_fixture", "action": "drag", "path": [{"x": 10, "y": 20}, {"x": 30, "y": 40}]}),
    ("keyboard text", "browser.action", {"browser_id": "browser_fixture", "action": "type", "text": "Hello"}),
    ("keyboard chord", "browser.action", {"browser_id": "browser_fixture", "action": "press", "key": "Control+a"}),
]:
    tool = tools[name]
    cases.append({"label": label, "function": {"name": name, "description": tool["description"], "parameters": tool["input_schema"]}, "arguments": arguments})

source = args.runtime.resolve() / "llama.cpp"
lib = args.runtime.resolve() / "build/bin"
with tempfile.TemporaryDirectory(prefix="ww-native-schema-") as temp:
    binary = Path(temp) / "probe"
    subprocess.run([
        "c++", "-std=c++17", "-O0", str(Path(__file__).with_suffix(".cpp")),
        *[f"-I{source / part}" for part in ["common", "vendor", "include", "ggml/include", "src"]],
        str(lib / "libllama-common.so.0"), str(lib / "libllama.so.0"),
        f"-Wl,-rpath,{lib}", "-o", str(binary),
    ], check=True)
    subprocess.run([str(binary)], input=json.dumps({"template": template, "cases": cases}), text=True, check=True)
