"""Unit tests for the documentation command and link validator."""

from __future__ import annotations

import importlib.util
import shutil
import sys
import tempfile
import unittest
from pathlib import Path
from types import ModuleType


def load_validator() -> ModuleType:
    path = Path(__file__).parents[1] / "validate-docs.py"
    spec = importlib.util.spec_from_file_location("validate_docs", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load validator from {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


validator = load_validator()


class CommandPathTests(unittest.TestCase):
    def test_command_path_plain_command_resolves_to_one_segment(self) -> None:
        tokens = ["firestone", "start", "dev", "--no-wait"]

        self.assertEqual(validator.command_path(tokens), (["start"], 1))

    def test_command_path_global_options_skipped_before_command(self) -> None:
        tokens = ["firestone", "--home", "/tmp/home", "--json", "ls"]

        self.assertEqual(validator.command_path(tokens), (["ls"], 4))

    def test_command_path_images_group_resolves_subcommand(self) -> None:
        tokens = ["firestone", "images", "pull", "ubuntu:24.04"]

        self.assertEqual(validator.command_path(tokens), (["images", "pull"], 1))

    def test_command_path_system_group_resolves_subcommand(self) -> None:
        tokens = ["firestone", "system", "prune", "--dry-run"]

        self.assertEqual(validator.command_path(tokens), (["system", "prune"], 1))

    def test_command_path_snapshot_group_resolves_subcommand(self) -> None:
        tokens = ["firestone", "snapshot", "restore", "dev", "snap-1", "--start"]

        self.assertEqual(validator.command_path(tokens), (["snapshot", "restore"], 1))

    def test_command_path_group_without_subcommand_rejected(self) -> None:
        tokens = ["firestone", "snapshot"]

        with self.assertRaises(validator.DocumentationError) as raised:
            validator.command_path(tokens)

        self.assertIn("snapshot invocation has no subcommand", str(raised.exception))


class ShellBlockTests(unittest.TestCase):
    def setUp(self) -> None:
        directory = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, directory)
        self.directory = Path(directory)

    def write(self, text: str) -> Path:
        document = self.directory / "doc.md"
        document.write_text(text, encoding="utf-8")
        return document

    def test_shell_blocks_curl_line_naming_firestone_is_not_a_command(self) -> None:
        document = self.write(
            "# T\n\n```sh\n"
            "curl -fsSL https://example.invalid/0xchasercat/firestone/install.sh | sh\n"
            "```\n"
        )

        self.assertEqual(validator.shell_blocks(document), [])

    def test_shell_blocks_text_fence_ignored(self) -> None:
        document = self.write("# T\n\n```text\nfirestone metrics dev\n```\n")

        self.assertEqual(validator.shell_blocks(document), [])

    def test_shell_blocks_firestone_line_collected_with_its_line_number(self) -> None:
        document = self.write("# T\n\n```sh\nfirestone ls --json\n```\n")

        self.assertEqual(validator.shell_blocks(document), [(4, "firestone ls --json")])


if __name__ == "__main__":
    unittest.main()
