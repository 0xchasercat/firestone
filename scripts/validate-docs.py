#!/usr/bin/env python3
"""Validate local Markdown links and Firestone commands against generated help."""

from __future__ import annotations

import argparse
import re
import shlex
import subprocess
import sys
from pathlib import Path
from urllib.parse import unquote, urlsplit

REPO_ROOT = Path(__file__).resolve().parents[1]
MARKDOWN_LINK = re.compile(r"!?\[[^\]]*\]\(([^)]+)\)")
HEADING = re.compile(r"^#{1,6}\s+(.+?)\s*#*\s*$")
FIRESTONE_TOKEN = re.compile(r"^firestone(?:\s|$)")
SHELL_OPERATORS = {"&", "&&", "|", "||", ";", ">", ">>", "2>", "2>>"}
GLOBAL_VALUE_OPTIONS = {"--home"}
GLOBAL_SWITCH_OPTIONS = {
    "--json",
    "--quiet",
    "-q",
    "--verbose",
    "-v",
    "--no-color",
    "--yes",
    "-y",
}
SUBCOMMAND_GROUPS = {"images", "snapshot", "system"}


class DocumentationError(RuntimeError):
    """A guide command or link is invalid."""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "paths",
        nargs="+",
        type=Path,
        help="Markdown files to validate",
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path("target/debug/firestone"),
        help="Firestone binary used for generated help",
    )
    return parser.parse_args()


def github_slug(value: str) -> str:
    value = re.sub(r"<[^>]+>", "", value.strip().lower())
    value = re.sub(r"[^\w\- ]", "", value, flags=re.UNICODE)
    return re.sub(r"\s+", "-", value)


def heading_anchors(path: Path) -> set[str]:
    anchors: set[str] = set()
    counts: dict[str, int] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        match = HEADING.match(line)
        if match is None:
            continue
        base = github_slug(match.group(1))
        count = counts.get(base, 0)
        counts[base] = count + 1
        anchors.add(base if count == 0 else f"{base}-{count}")
    return anchors


def local_link_target(source: Path, raw_target: str) -> tuple[Path, str] | None:
    target = raw_target.strip()
    if target.startswith("<") and target.endswith(">"):
        target = target[1:-1]
    target = target.split(maxsplit=1)[0]
    parsed = urlsplit(target)
    if parsed.scheme or parsed.netloc:
        return None
    relative = unquote(parsed.path)
    destination = source if not relative else source.parent / relative
    return destination.resolve(), unquote(parsed.fragment)


def validate_links(paths: list[Path]) -> int:
    anchors: dict[Path, set[str]] = {}
    checked = 0
    root = REPO_ROOT.resolve()
    for source in paths:
        text = source.read_text(encoding="utf-8")
        for match in MARKDOWN_LINK.finditer(text):
            resolved = local_link_target(source, match.group(1))
            if resolved is None:
                continue
            destination, anchor = resolved
            try:
                destination.relative_to(root)
            except ValueError as error:
                raise DocumentationError(
                    f"{source}: local link escapes the repository: {match.group(1)!r}"
                ) from error
            if not destination.exists():
                raise DocumentationError(
                    f"{source}: local link does not exist: {match.group(1)!r}"
                )
            if anchor:
                if not destination.is_file():
                    raise DocumentationError(
                        f"{source}: link anchor targets a non-file: {match.group(1)!r}"
                    )
                destination_anchors = anchors.setdefault(
                    destination,
                    heading_anchors(destination),
                )
                if anchor not in destination_anchors:
                    raise DocumentationError(
                        f"{source}: link anchor does not exist: {match.group(1)!r}"
                    )
            checked += 1
    return checked


def shell_blocks(path: Path) -> list[tuple[int, str]]:
    commands: list[tuple[int, str]] = []
    in_shell = False
    logical = ""
    logical_line = 0
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if line.startswith("```"):
            language = line[3:].strip().lower()
            if in_shell:
                in_shell = False
                logical = ""
            else:
                in_shell = language in {"sh", "bash", "shell"}
            continue
        if not in_shell:
            continue
        stripped = line.strip()
        if not logical:
            logical_line = line_number
        if stripped.endswith("\\"):
            logical += stripped[:-1] + " "
            continue
        logical += stripped
        if FIRESTONE_TOKEN.search(logical):
            commands.append((logical_line, logical))
        logical = ""
    if logical:
        raise DocumentationError(f"{path}:{logical_line}: unterminated shell continuation")
    return commands


def command_segment(line: str) -> list[str]:
    try:
        tokens = shlex.split(line, comments=True, posix=True)
    except ValueError as error:
        raise DocumentationError(f"cannot parse shell command {line!r}: {error}") from error
    firestone_index = next(
        (index for index, token in enumerate(tokens) if token == "firestone"),
        None,
    )
    if firestone_index is None:
        raise DocumentationError(f"cannot find Firestone executable in {line!r}")
    result = []
    for token in tokens[firestone_index:]:
        if token in SHELL_OPERATORS:
            break
        result.append(token)
    return result


def command_path(tokens: list[str]) -> tuple[list[str], int]:
    index = 1
    while index < len(tokens):
        token = tokens[index]
        if token in GLOBAL_VALUE_OPTIONS:
            index += 2
            continue
        if token in GLOBAL_SWITCH_OPTIONS or token.startswith("-v"):
            index += 1
            continue
        if token.startswith("-"):
            raise DocumentationError(
                f"cannot resolve command because global option is unknown: {token}"
            )
        break
    if index >= len(tokens):
        raise DocumentationError(f"Firestone invocation has no command: {tokens!r}")
    path = [tokens[index]]
    command_index = index
    if tokens[index] in SUBCOMMAND_GROUPS:
        group = tokens[index]
        index += 1
        while index < len(tokens) and tokens[index].startswith("-"):
            index += 1
        if index >= len(tokens):
            raise DocumentationError(f"{group} invocation has no subcommand: {tokens!r}")
        path.append(tokens[index])
    return path, command_index


def option_tokens(tokens: list[str]) -> list[str]:
    options = []
    for token in tokens[1:]:
        if token == "--":
            break
        if token.startswith("--"):
            options.append(token.split("=", 1)[0])
        elif token.startswith("-") and token != "-":
            options.append(token)
    return options


def help_for(binary: Path, path: list[str]) -> str:
    completed = subprocess.run(
        [binary, *path, "--help"],
        cwd=REPO_ROOT,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=15,
        check=False,
    )
    if completed.returncode != 0:
        detail = (completed.stdout + completed.stderr).decode("utf-8", errors="replace")
        raise DocumentationError(
            f"generated help failed for {' '.join(path)}: {detail.strip()}"
        )
    if completed.stderr:
        raise DocumentationError(f"generated help wrote stderr for {' '.join(path)}")
    return completed.stdout.decode("utf-8", errors="strict")


def validate_commands(paths: list[Path], binary: Path) -> int:
    cache: dict[tuple[str, ...], str] = {}
    checked = 0
    for source in paths:
        for line_number, line in shell_blocks(source):
            tokens = command_segment(line)
            path, _ = command_path(tokens)
            key = tuple(path)
            help_text = cache.setdefault(key, help_for(binary, path))
            for option in option_tokens(tokens):
                if option.startswith("-vv"):
                    option = "-v"
                pattern = rf"(?<![\w-]){re.escape(option)}(?![\w-])"
                if re.search(pattern, help_text) is None:
                    raise DocumentationError(
                        f"{source}:{line_number}: {option} is absent from "
                        f"`firestone {' '.join(path)} --help`"
                    )
            checked += 1
    return checked


def main() -> int:
    args = parse_args()
    paths = [path.resolve() for path in args.paths]
    for path in paths:
        if not path.is_file():
            raise DocumentationError(f"Markdown path is not a file: {path}")
    binary = args.binary.resolve()
    if not binary.is_file():
        raise DocumentationError(f"Firestone binary is missing: {binary}")
    links = validate_links(paths)
    commands = validate_commands(paths, binary)
    print(f"validated {commands} Firestone commands and {links} local Markdown links")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (DocumentationError, OSError, UnicodeError, subprocess.SubprocessError) as error:
        print(f"documentation validation failed: {error}", file=sys.stderr)
        raise SystemExit(1) from error
