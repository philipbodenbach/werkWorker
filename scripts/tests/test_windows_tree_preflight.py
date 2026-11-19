import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "validate-windows-process-tree.py"
SPEC = importlib.util.spec_from_file_location("windows_tree_validator", SCRIPT)
VALIDATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VALIDATOR)


class HostPreflightTests(unittest.TestCase):
    def test_missing_ordinary_logon_is_an_infrastructure_block(self):
        refused = {"exit_code": 125, "native_host_token": {"mode": "launch_refused",
                   "error": "No linked Medium token"}}
        with tempfile.TemporaryDirectory() as temporary:
            evidence = Path(temporary) / "preflight"
            with patch.object(VALIDATOR, "execute", return_value=refused):
                status = VALIDATOR.preflight(["--evidence", str(evidence), "--require-psec"])
            result = json.loads((evidence / "preflight-result.json").read_text(encoding="utf-8"))
        self.assertEqual(status, 125)
        self.assertEqual(result["status"], "INFRASTRUCTURE_BLOCKED")
        self.assertEqual(result["error"], "No linked Medium token")
        self.assertNotIn("host", result)

    def test_successful_launcher_requires_an_actual_host_observation(self):
        with tempfile.TemporaryDirectory() as temporary:
            evidence = Path(temporary) / "preflight"
            with patch.object(VALIDATOR, "execute", return_value={"exit_code": 0}):
                status = VALIDATOR.preflight(["--evidence", str(evidence)])
            result = json.loads((evidence / "preflight-result.json").read_text(encoding="utf-8"))
        self.assertEqual(status, 125)
        self.assertEqual(result["status"], "INFRASTRUCTURE_BLOCKED")

    def test_psec_support_on_a_server_or_emulated_host_is_insufficient(self):
        client = {"windows_build": 26200, "workstation": True, "native_machine": 0x8664,
                  "process_machine": 0, "psec": {"supports_required_policy": True}}
        self.assertTrue(VALIDATOR.is_psec_qualification_host(client))
        for changes in ({"workstation": False}, {"native_machine": 0xAA64, "process_machine": 0x8664},
                        {"psec": {"supports_required_policy": False}}):
            with self.subTest(changes=changes):
                self.assertFalse(VALIDATOR.is_psec_qualification_host(dict(client, **changes)))
