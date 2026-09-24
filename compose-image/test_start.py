import importlib.util
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("compose_start", Path(__file__).with_name("start.py"))
start = importlib.util.module_from_spec(spec)
spec.loader.exec_module(start)


class StartupTests(unittest.TestCase):
    def services(self):
        return [{"driveID": f"compose_{i}", "mountPath": f"/mnt/compose_{i}",
                 "localImage": f"aenv-compose/service-{i}:local", "config": {}}
                for i in range(2)]

    def test_reject_unmounted_or_shared_rootfs(self):
        with patch.object(start.os.path, "ismount", return_value=False), patch.object(Path, "is_symlink", return_value=False):
            with self.assertRaisesRegex(ValueError, "not mounted"):
                start.validate_drives(self.services())
        with patch.object(start.os.path, "ismount", return_value=True), patch.object(Path, "is_symlink", return_value=False), patch.object(Path, "stat", return_value=SimpleNamespace(st_dev=1)):
            with self.assertRaisesRegex(ValueError, "not isolated"):
                start.validate_drives(self.services())

    def test_runtime_environment_cannot_fill_unresolved_variables(self):
        env = start.compose_environment({"services": {"web": {"environment": {"HOME": None, "PATH": None}}}})
        self.assertNotIn("HOME", env)
        self.assertNotIn("PATH", env)
        self.assertEqual(env["COMPOSE_DISABLE_ENV_FILE"], "1")


if __name__ == "__main__":
    unittest.main()
