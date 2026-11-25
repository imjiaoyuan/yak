import os
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest.mock import patch


class ExtensionLayoutTests(unittest.TestCase):
    def test_skills_load_collection_roots_and_skill_subdirectories(self):
        module = runpy.run_path(str(Path(__file__).resolve().parents[1] / "examples/extensions/skills"))
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            for relative, name in (
                ("extensions/skills/local/custom", "custom"),
                ("extensions/skills/repos/academic/paper", "paper"),
                ("extensions/skills/repos/nature/skills/figure", "figure"),
                ("extensions/skills/repos/nature/examples/example", "example"),
            ):
                directory = root / relative
                directory.mkdir(parents=True)
                (directory / "SKILL.md").write_text(f"---\nname: {name}\ndescription: A sample skill.\n---\nBody\n")
            with patch.object(Path, "home", return_value=root):
                skills = module["discover"](str(root), root, {"figure"})
            self.assertEqual({skill["name"] for skill in skills}, {"custom", "paper"})

    def test_commands_use_plugin_owned_templates(self):
        module = runpy.run_path(str(Path(__file__).resolve().parents[1] / "examples/extensions/commands"))
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            directory = root / "extensions/commands/templates"
            directory.mkdir(parents=True)
            (directory / "sample.md").write_text("Sample prompt")
            self.assertIn("sample", module["names"](str(root), str(root)))

    def test_model_pin_uses_plugin_owned_state(self):
        module = runpy.run_path(str(Path(__file__).resolve().parents[1] / "examples/extensions/model_pin"))
        with tempfile.TemporaryDirectory() as scratch:
            with patch.dict(os.environ, {"YAK_USER_PATH": scratch}):
                self.assertEqual(Path(module["pins_path"]()), Path(scratch) / "extensions/model_pin/pins.json")


if __name__ == "__main__":
    unittest.main()
