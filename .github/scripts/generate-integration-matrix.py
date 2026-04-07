#!/usr/bin/env python3
"""Generate a GitHub Actions matrix JSON from a nextest archive.

Lists all integration tests (non-lib tests) from ferrex-core and ferrex-server,
emitting one matrix entry per test so CI can fan them out into parallel jobs.
"""

import json
import subprocess
import sys


def main() -> None:
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <archive-path>", file=sys.stderr)
        sys.exit(1)

    archive_path = sys.argv[1]

    try:
        result = subprocess.run(
            [
                "cargo",
                "nextest",
                "list",
                "--archive-file",
                archive_path,
                "--message-format",
                "json",
            ],
            capture_output=True,
            text=True,
            check=True,
        )
    except subprocess.CalledProcessError as e:
        print(f"Failed to list tests: {e.stderr}", file=sys.stderr)
        sys.exit(1)

    data = json.loads(result.stdout)
    suites = data.get("rust-suites", {})

    matrix = []
    for binary_id, suite in suites.items():
        # Integration test binaries have "::" in the name (e.g. ferrex-core::store_recall).
        # Lib test binaries are just the package name — skip them.
        if "::" not in binary_id:
            continue
        testcases = suite.get("testcases", {})
        for test_name in testcases:
            matrix.append(
                {
                    "binary": binary_id,
                    "test": test_name,
                }
            )

    print(json.dumps(matrix))


if __name__ == "__main__":
    main()
