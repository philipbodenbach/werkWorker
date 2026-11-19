import copy
import importlib.util
import json
from pathlib import Path
from types import SimpleNamespace
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "validate-windows-process-tree.py"
SPEC = importlib.util.spec_from_file_location("installed_tree", SCRIPT)
validator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validator)


class InstalledWorkloads(unittest.TestCase):
    def test_list_receipt_is_bound_to_one_exact_request(self):
        turn = validator.tool_turn("process.run", {"argv": ["probe.exe", "--list"], "cwd": "fresh"})
        request = turn["deltas"][0]
        item = {"call": {"id": request["id"], "arguments": json.loads(request["arguments"])},
                "result": {"data": {"executed": True, "stdout": validator.ORIGINAL_PROBE_LIST + "\r\n"}}}

        def verdict(calls):
            return validator.workload_verdict(
                calls, turn, lambda row: row["result"]["data"]["executed"] is True,
                lambda output: output.splitlines() == validator.ORIGINAL_PROBE_LIST.splitlines())

        self.assertEqual(verdict([item]), "PASS")
        self.assertEqual(verdict([]), "NOT_RUN")
        self.assertEqual(verdict([item, item]), "FAIL")
        swapped = copy.deepcopy(item)
        swapped["call"]["arguments"]["argv"] = ["cargo.exe", "--version"]
        self.assertEqual(verdict([swapped]), "FAIL")
        item["result"]["data"]["stdout"] = "8 tests, 0 benchmarks"
        self.assertEqual(verdict([item]), "FAIL")
        item["result"]["data"]["executed"] = False
        self.assertEqual(verdict([item]), "NOT_RUN")

    def test_wrong_probe_refuses_before_workspace_or_cli(self):
        with TemporaryDirectory() as temp:
            root = Path(temp)
            wrong = root / "wrong.exe"
            wrong.write_bytes(b"not the reference probe")
            manifest = root / "manifest.json"
            manifest.write_text("{}", encoding="utf-8")
            args = SimpleNamespace(installed_bin=root, manifest=manifest, rust_probe=wrong,
                                   evidence=root, source=root)
            with patch.object(validator, "execute", side_effect=AssertionError("Unexpected launch")):
                self.assertEqual(validator.installed_start(args, {}), 1)
            receipt = json.loads((root / "installed-start-result.json").read_text(encoding="utf-8"))
            self.assertEqual(receipt["stage"], "original_probe_input")
            self.assertEqual(receipt["status"], "NOT_RUN")
            self.assertFalse(receipt["passed"])
