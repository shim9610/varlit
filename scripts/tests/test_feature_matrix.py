import importlib.util
from pathlib import Path
import json
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "feature_matrix", Path(__file__).resolve().parents[1] / "feature_matrix.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


class FeatureDiscoveryTests(unittest.TestCase):
    def test_cargo_implicit_optional_dependency_feature_is_discovered(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text('[workspace]\nmembers=["app", "helper"]\nresolver="3"\n')
            for folder, package in [("app", "varlit"), ("helper", "helper")]:
                (root / folder / "src").mkdir(parents=True)
                (root / folder / "src/lib.rs").write_text("")
                manifest = f'[package]\nname="{package}"\nversion="0.0.0"\nedition="2024"\n'
                if folder == "app":
                    manifest += '[dependencies]\noptional-codec={package="helper",path="../helper",optional=true}\n'
                (root / folder / "Cargo.toml").write_text(manifest)
            metadata = json.loads(subprocess.check_output(
                ["cargo", "metadata", "--offline", "--no-deps", "--format-version", "1"], cwd=root, text=True))
            cases = gate.feature_matrix({package["name"]: package for package in metadata["packages"]})
            self.assertIn("varlit-optional-codec", [entry["name"] for entry in cases["singletons"]])

    def test_new_facade_and_core_only_features_join_automatically(self):
        cases = gate.feature_matrix({
            "varlit": {"features": {"default": [], "new-codec": ["varlit-core/new-codec"]}},
            "varlit-core": {"features": {"new-codec": [], "private-mode": []}},
            "varlit-macros": {"features": {"new-syntax": []}},
        })
        self.assertEqual({entry["name"] for entry in cases["singletons"]}, {
            "varlit-new-codec", "varlit-core-private-mode", "varlit-macros-new-syntax",
        })
        self.assertEqual(len(cases["checks"]), len(cases["singletons"]) + 3)

    def test_indirect_forward_does_not_silently_skip_a_core_configuration(self):
        cases = gate.feature_matrix({
            "varlit": {"features": {"all-codecs": ["varlit-core/new-codec"]}},
            "varlit-core": {"features": {"new-codec": []}},
        })
        self.assertEqual(len(cases["singletons"]), 2)

    def test_matrix_arguments_cannot_inject_shell_commands(self):
        for name in ["a b", "a;exit", "$(id)", "a\nb"]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                gate.feature_matrix({"varlit": {"features": {name: []}}})
