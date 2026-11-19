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


class PreparedInputsTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.installed = self.root / "runner"
        self.installed.write_bytes(b"package fixture")
        self.token = dict(user_sid="S-1-test-host", session_id=4, token_type=1,
                          elevated=False, integrity_rid=0x2000, appcontainer=False)
        self.manifests = []
        for name in ("suite", "cli"):
            runtime = self.root / name
            runtime.mkdir()
            workspace = self.root / (name + "-workspace")
            workspace.mkdir()
            runner = runtime / "ww-windows-runner.exe"
            runner.write_bytes(self.installed.read_bytes())
            path = runtime / "provisioning.json"
            path.write_text(json.dumps(dict(host_sid=self.token["user_sid"], session_id=4,
                runner=str(runner), runner_sha256=VALIDATOR.digest(runner), workspace=str(workspace),
                user_sid=name)))
            self.manifests.append(path)

    def receipts(self):
        return VALIDATOR.prepared_manifests(self.manifests, self.token, self.installed)

    def test_separate_inputs_match_installed_runner(self):
        for receipt, path in zip(self.receipts(), self.manifests, strict=True):
            self.assertEqual(receipt["manifest_sha256"], VALIDATOR.digest(path))
            self.assertEqual(receipt["runner_sha256"], VALIDATOR.digest(self.installed))

    def test_second_input_refused_before_either_workload(self):
        path = self.manifests[1]
        original = json.loads(path.read_bytes())
        first = json.loads(self.manifests[0].read_bytes())
        for changes in ({"session_id": 3}, {"session_id": None}, {"host_sid": "admin"},
                        {"runner_sha256": "0" * 64}, {"workspace": first["workspace"]},
                        {"user_sid": first["user_sid"]}):
            with self.subTest(changes=changes):
                path.write_text(json.dumps(dict(original, **changes)))
                with self.assertRaises(ValueError):
                    self.receipts()

    def test_stale_workspace_and_runner_replacement(self):
        manifest = json.loads(self.manifests[0].read_bytes())
        stale = Path(manifest["workspace"]) / "previous-output"
        stale.touch()
        with self.assertRaisesRegex(ValueError, "empty workspace"):
            self.receipts()
        stale.unlink()
        Path(manifest["runner"]).write_bytes(b"different build")
        with self.assertRaisesRegex(ValueError, "differs from package"):
            self.receipts()

    def test_admin_is_refused_even_with_matching_sid(self):
        self.token["elevated"] = True
        with self.assertRaisesRegex(RuntimeError, "non-elevated"):
            self.receipts()
