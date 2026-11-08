# SPDX-License-Identifier: Apache-2.0
import importlib.util
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "release" / "prepare-native-semantic.py"
SPEC = importlib.util.spec_from_file_location("prepare_native_semantic", SCRIPT)
native = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(native)


class RuntimeFilesTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory(dir=ROOT)
        self.addCleanup(self.temp_dir.cleanup)
        self.ort_source = Path(self.temp_dir.name)

    def make_build(self, output_directory, files):
        build_root = self.ort_source / "build" / "Release"
        output = build_root / output_directory
        output.mkdir(parents=True)
        for name in files:
            (output / name).write_bytes(b"fixture")
        (build_root / "CMakeCache.txt").write_text(
            "onnxruntime_USE_TELEMETRY:BOOL=OFF\n"
            "onnxruntime_BUILD_SHARED_LIB:BOOL=ON\n"
            "onnxruntime_BUILD_UNIT_TESTS:BOOL=OFF\n",
            encoding="utf-8",
        )

    def test_linux_build_dir_is_config_subdirectory(self):
        self.make_build(".", ["libonnxruntime.so.1.24.4", "libonnxruntime_providers_shared.so"])
        with patch.object(native.sys, "platform", "linux"):
            platform, main, companion = native.runtime_files(self.ort_source)
        self.assertEqual(platform, "linux-x86_64")
        self.assertEqual(main.name, "libonnxruntime.so.1.24.4")
        self.assertEqual(companion.name, "libonnxruntime_providers_shared.so")

    def test_linux_still_requires_companion_library(self):
        self.make_build(".", ["libonnxruntime.so.1.24.4"])
        with patch.object(native.sys, "platform", "linux"):
            with self.assertRaisesRegex(SystemExit, "expected one companion runtime library"):
                native.runtime_files(self.ort_source)

    def test_macos_build_dir_is_config_subdirectory(self):
        self.make_build(".", ["libonnxruntime.1.24.4.dylib"])
        with patch.object(native.sys, "platform", "darwin"):
            platform, main, companion = native.runtime_files(self.ort_source)
        self.assertEqual(platform, "macos-x86_64")
        self.assertEqual(main.name, "libonnxruntime.1.24.4.dylib")
        self.assertIsNone(companion)

    def test_windows_multiconfig_output_is_found_below_config_dir(self):
        self.make_build("Release", ["onnxruntime.dll", "onnxruntime_providers_shared.dll"])
        with patch.object(native.sys, "platform", "win32"):
            platform, main, companion = native.runtime_files(self.ort_source)
        self.assertEqual(platform, "windows-x86_64")
        self.assertEqual(main.name, "onnxruntime.dll")
        self.assertEqual(companion.name, "onnxruntime_providers_shared.dll")
        self.assertEqual(main.parent, companion.parent)

    def test_windows_still_requires_companion_library(self):
        self.make_build("Release", ["onnxruntime.dll"])
        with patch.object(native.sys, "platform", "win32"):
            with self.assertRaisesRegex(SystemExit, "expected one companion runtime library"):
                native.runtime_files(self.ort_source)


class NativeBuildConfigurationTests(unittest.TestCase):
    def test_windows_uses_installed_visual_studio_2026_generator(self):
        with patch.object(native.sys, "platform", "win32"):
            args = native.ort_build_args()
        self.assertEqual(args[args.index("--cmake_generator") + 1], "Visual Studio 18 2026")

    def test_build_disables_onnxruntime_unit_test_targets(self):
        args = native.ort_build_args()
        self.assertIn("onnxruntime_BUILD_UNIT_TESTS=OFF", args)

    def test_windows_suppresses_only_upstream_c4875_warning(self):
        with (
            patch.object(native.sys, "platform", "win32"),
            patch.dict(native.os.environ, {"_CL_": "/DKEEP"}, clear=False),
        ):
            env = native.ort_build_env()
        self.assertEqual(env["_CL_"], "/DKEEP /wd4875")

    def test_other_platforms_do_not_receive_msvc_warning_flags(self):
        with patch.object(native.sys, "platform", "darwin"):
            self.assertIsNone(native.ort_build_env())

    def test_workflow_marker_matches_native_test_output(self):
        marker = "native direct outbound probe denied by process isolation:"
        rust_source = (ROOT / "crates/cli/src/knowledge_vector.rs").read_text(encoding="utf-8")
        workflow = (ROOT / ".github/workflows/semantic-native.yml").read_text(encoding="utf-8")
        self.assertIn(marker, rust_source)
        self.assertEqual(workflow.count(marker), 3)


if __name__ == "__main__":
    unittest.main()
