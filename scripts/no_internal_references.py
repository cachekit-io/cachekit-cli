"""Reject internal references in docs and commit messages.

The patterns are generic on purpose: a list of specific internal names would itself
disclose them. Scope rule: https://docs.cachekit.io/contributing/#what-belongs-in-these-docs
"""

import argparse
import re
import subprocess
import sys
from collections.abc import Iterator

PATTERN = re.compile(
    r"LAB-[0-9]+|op://|\.ts\.net|(?i:k3s cluster)|dev\.cachekit\.io"
    r"|\bStage [0-9]|\bAC-[0-9]|github\.com/cachekit(?![\w-])"
)
# `git commit -v` appends the staged diff below this line, and the commit-msg hook
# sees it: git strips it only after the hook runs.
CUT_LINE = "------------------------ >8 ------------------------"


def comment_char() -> str:
    value = subprocess.run(
        ["git", "config", "core.commentChar"], capture_output=True, text=True
    ).stdout.strip()
    # "auto" picks a character per commit, starting from "#".
    return value if value and value != "auto" else "#"


def message_lines(lines: list[str]) -> Iterator[tuple[int, str]]:
    """The lines git keeps: up to the cut line, minus comments and a Merge subject.

    Comments and the Merge subject quote branch names, which carry ticket ids.
    """
    comment = comment_char()
    for lineno, line in enumerate(lines, 1):
        if line.rstrip("\n") == f"{comment} {CUT_LINE}":
            return
        if line.startswith(comment) or (lineno == 1 and line.startswith("Merge ")):
            continue
        yield lineno, line


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--commit-msg",
        action="store_true",
        help="check a commit message as git will record it",
    )
    parser.add_argument("files", nargs="+")
    args = parser.parse_args()

    found = False
    for path in args.files:
        with open(path, encoding="utf-8", errors="replace") as f:
            lines = f.readlines()
        for lineno, line in (
            message_lines(lines) if args.commit_msg else enumerate(lines, 1)
        ):
            if PATTERN.search(line):
                print(f"{path}:{lineno}:{line.rstrip()}")
                found = True
    return int(found)


if __name__ == "__main__":
    sys.exit(main())
