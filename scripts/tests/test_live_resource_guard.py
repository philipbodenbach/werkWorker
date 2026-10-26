import unittest
from unittest import mock
import importlib.util
from pathlib import Path
import sys
from live_resource_guard import pressure


class ResourceAdmissionTests(unittest.TestCase):
    def test_import_and_admission_do_not_require_posix_locking(self):
        spec = importlib.util.spec_from_file_location(
            'guard_without_fcntl', Path(__file__).with_name('live_resource_guard.py'))
        module = importlib.util.module_from_spec(spec)
        with mock.patch.dict(sys.modules, {'fcntl': None}):
            spec.loader.exec_module(module)
            self.assertIn('GPU', module.pressure(self.state(), 8192, 2048, 2048, True))

    def test_unsupported_platform_refuses_before_launch_or_linux_imports(self):
        import live_resource_guard as guard
        for platform in ['win32', 'darwin']:
            with self.subTest(platform=platform), \
                    mock.patch.object(sys, 'platform', platform), \
                    mock.patch.object(sys, 'argv', ['guard', '--report', 'unused', '--', 'unused']), \
                    mock.patch.dict(sys.modules, {'fcntl': None}), \
                    mock.patch.object(guard.subprocess, 'Popen') as launch, \
                    mock.patch.object(guard, 'sample') as sample, \
                    mock.patch('sys.stderr'):
                with self.assertRaises(SystemExit) as stopped:
                    guard.main()
                self.assertEqual(stopped.exception.code, 2)
                launch.assert_not_called()
                sample.assert_not_called()

    def state(self):
        return dict(available_ram_mib=75000, used_swap_mib=0, test_rss_mib=100,
                    gpus=[dict(index=0, free_mib=500)])

    def test_idle_system_with_nearly_full_gpu_does_not_admit_inference(self):
        self.assertIn('GPU', pressure(self.state(), 8192, 2048, 2048, True))

    def test_memory_growth_stops_only_when_test_group_exceeds_its_allowance(self):
        state = self.state()
        state['gpus'][0]['free_mib'] = 4096
        self.assertIsNone(pressure(state, 8192, 2048, 2048))
        state['test_rss_mib'] = 2049
        self.assertIn('RSS', pressure(state, 8192, 2048, 2048))

    def test_ram_pressure_blocks_even_without_a_local_gpu(self):
        state = self.state()
        state.update(gpus=[], available_ram_mib=4000)
        self.assertIn('RAM', pressure(state, 8192, 2048, 2048))

    def test_missing_gpu_metrics_fail_closed_when_required(self):
        state = self.state()
        state['gpus'] = []
        self.assertIsNotNone(pressure(state, 8192, 2048, 2048, True))
        self.assertIsNone(pressure(state, 8192, 2048, 2048, False))

    def test_partial_gpu_metrics_do_not_hide_an_unmeasured_device(self):
        state = self.state()
        state['gpus'][0]['free_mib'] = 4096
        state['gpu_metrics_error'] = 'ValueError'
        self.assertIsNotNone(pressure(state, 8192, 2048, 2048, True))
