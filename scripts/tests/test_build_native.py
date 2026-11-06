"""Standalone builds must use the selected SDK and leave source manifests intact."""

import importlib.util
from pathlib import Path
import tempfile
import tomllib
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "build-native.py"
SPEC = importlib.util.spec_from_file_location("build_native", SCRIPT)
build_native = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(build_native)


class StandaloneManifestTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.sdk = self.root / "selected sdk"
        self.plugin = self.root / "independent plugin"
        (self.sdk / "crates/module-sdk").mkdir(parents=True)
        (self.plugin / "src").mkdir(parents=True)
        (self.sdk / "Cargo.toml").write_text(
            '[workspace]\nmembers=[]\n'
            '[workspace.package]\nversion="1.2.3"\nedition="2024"\n'
            '[workspace.dependencies]\namaterasu-module-sdk={path="crates/module-sdk"}\n'
            'serde={version="1",features=["derive"]}\n'
            '[workspace.lints.rust]\nunsafe_op_in_unsafe_fn="deny"\n'
        )
        (self.sdk / "crates/module-sdk/Cargo.toml").write_text(
            '[package]\nname="amaterasu-module-sdk"\nversion="1.0.0"\n'
        )
        (self.plugin / "Cargo.toml").write_text(
            '[package]\nname="amaterasu-remote-session"\nversion.workspace=true\nedition.workspace=true\n'
            '[lib]\ncrate-type=["cdylib","rlib"]\n'
            '[dependencies]\nsdk={package="amaterasu-module-sdk",path="../../old-sdk/crates/module-sdk"}\n'
            'serde={workspace=true,features=["std"]}\n'
            '[target.\'cfg(windows)\'.dependencies]\namaterasu-module-sdk={workspace=true}\n'
            '[lints]\nworkspace=true\n'
        )
        (self.plugin / "src/lib.rs").write_text("pub fn example() {}\n")

    def test_selected_sdk_identity_is_used_for_renamed_and_target_dependencies(self):
        original_plugin = (self.plugin / "Cargo.toml").read_bytes()
        original_sdk = (self.sdk / "Cargo.toml").read_bytes()
        manifest = build_native.standalone_manifest(self.plugin, self.sdk)
        expected = str(self.sdk / "crates/module-sdk")
        self.assertEqual(manifest["dependencies"]["sdk"]["path"], expected)
        self.assertEqual(manifest["target"]["cfg(windows)"]["dependencies"]["amaterasu-module-sdk"]["path"], expected)
        self.assertEqual(manifest["dependencies"]["serde"]["features"], ["derive", "std"])
        self.assertEqual(manifest["package"]["version"], "1.2.3")
        self.assertEqual(manifest["lints"]["rust"]["unsafe_op_in_unsafe_fn"], "deny")
        self.assertEqual(manifest["lib"]["path"], str(self.plugin / "src/lib.rs"))
        self.assertEqual((self.plugin / "Cargo.toml").read_bytes(), original_plugin)
        self.assertEqual((self.sdk / "Cargo.toml").read_bytes(), original_sdk)
        self.assertEqual(tomllib.loads(build_native.serialize_manifest(manifest)), manifest)

    def test_internal_path_and_explicit_tests_keep_their_source_paths(self):
        helper = self.plugin / "helper"
        helper.mkdir()
        (helper / "Cargo.toml").write_text('[package]\nname="helper"\nversion="0.1.0"\n')
        with (self.plugin / "Cargo.toml").open("a") as source:
            source.write('\n[dev-dependencies]\nhelper={path="helper"}\n')
        (self.plugin / "tests").mkdir()
        (self.plugin / "tests/provider.rs").write_text("#[test] fn probe() {}\n")
        manifest = build_native.standalone_manifest(self.plugin, self.sdk)
        self.assertEqual(manifest["dev-dependencies"]["helper"]["path"], str(helper))
        self.assertEqual(manifest["test"], [{"name": "provider", "path": str(self.plugin / "tests/provider.rs")}])

    def test_dependency_identity_mismatch_is_rejected(self):
        (self.sdk / "crates/module-sdk/Cargo.toml").write_text('[package]\nname="unexpected-package"\n')
        with self.assertRaisesRegex(ValueError, "wrong package identity"):
            build_native.standalone_manifest(self.plugin, self.sdk)

    def test_unknown_external_paths_are_rejected(self):
        with (self.plugin / "Cargo.toml").open("a") as source:
            source.write('\n[dev-dependencies]\nunselected={path="../unselected"}\n')
        with self.assertRaisesRegex(ValueError, "no unique SDK path dependency"):
            build_native.standalone_manifest(self.plugin, self.sdk)

    def test_only_declared_core_test_dependencies_are_omitted(self):
        with (self.plugin / "Cargo.toml").open("a") as source:
            source.write(
                '\n[package.metadata.amaterasu.standalone]\n'
                'core-test-dependencies=["state","runtime"]\n'
                '[features]\ndefault=["core-integration-tests","other"]\n'
                'core-integration-tests=[]\nother=[]\n'
                '[dev-dependencies]\nstate={path="../state"}\n'
                '[target.\'cfg(windows)\'.dev-dependencies]\nruntime={path="../runtime"}\n'
            )
        original = (self.plugin / "Cargo.toml").read_bytes()
        manifest = build_native.standalone_manifest(self.plugin, self.sdk)
        self.assertEqual(manifest["dev-dependencies"], {})
        self.assertEqual(manifest["target"]["cfg(windows)"]["dev-dependencies"], {})
        self.assertEqual(manifest["features"]["default"], ["other"])
        self.assertEqual(manifest["features"]["core-integration-tests"], [])
        self.assertEqual((self.plugin / "Cargo.toml").read_bytes(), original)

    def test_core_test_metadata_cannot_omit_production_dependencies(self):
        with (self.plugin / "Cargo.toml").open("a") as source:
            source.write(
                '\n[package.metadata.amaterasu.standalone]\n'
                'core-test-dependencies=["sdk"]\n'
                '[features]\ncore-integration-tests=[]\n'
            )
        with self.assertRaisesRegex(ValueError, "declared dev-dependencies"):
            build_native.standalone_manifest(self.plugin, self.sdk)


if __name__ == "__main__":
    unittest.main()
