#!/usr/bin/env python3

import argparse
import shlex
import shutil
import subprocess
import tempfile
from pathlib import Path


BENCHMARK_DIR = Path(__file__).resolve().parent
DEFAULT_FIXTURE = BENCHMARK_DIR / "fixtures" / "rlang-1.0.1"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark fresh rpx resolution against the live registry."
    )
    parser.add_argument(
        "--rpx-path",
        action="append",
        required=True,
        type=Path,
        help="Path to an rpx binary to benchmark; may be supplied more than once.",
    )
    parser.add_argument(
        "--warmup",
        type=int,
        default=1,
        help="Number of Hyperfine warmup runs (default: 1).",
    )
    parser.add_argument(
        "--runs",
        type=int,
        default=5,
        help="Number of measured Hyperfine runs (default: 5).",
    )
    parser.add_argument(
        "--export-json",
        type=Path,
        help="Optional path for Hyperfine JSON output.",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if args.warmup < 0:
        raise SystemExit("--warmup must be at least 0")
    if args.runs < 1:
        raise SystemExit("--runs must be at least 1")
    if shutil.which("hyperfine") is None:
        raise SystemExit("hyperfine is required but was not found on PATH")
    if not (DEFAULT_FIXTURE / "DESCRIPTION").is_file():
        raise SystemExit(f"fixture is missing: {DEFAULT_FIXTURE}")

    binaries = []
    for path in args.rpx_path:
        path = path.expanduser().resolve()
        if not path.is_file():
            raise SystemExit(f"rpx binary does not exist: {path}")
        binaries.append(path)

    with tempfile.TemporaryDirectory(prefix="rpx-benchmark-") as temporary_dir:
        temporary_dir = Path(temporary_dir)
        commands = []
        prepares = []

        for index, binary in enumerate(binaries):
            project_dir = temporary_dir / str(index)
            shutil.copytree(DEFAULT_FIXTURE, project_dir)
            prepares.append(f"rm -f {shlex.quote(str(project_dir / 'rpx.lock'))}")
            commands.append(
                f"cd {shlex.quote(str(project_dir))} && "
                f"{shlex.quote(str(binary))} lock"
            )

        hyperfine = [
            "hyperfine",
            "--warmup",
            str(args.warmup),
            "--runs",
            str(args.runs),
        ]
        if args.export_json is not None:
            hyperfine.extend(["--export-json", str(args.export_json.resolve())])
        for binary in binaries:
            hyperfine.extend(["--command-name", str(binary)])
        for prepare in prepares:
            hyperfine.extend(["--prepare", prepare])
        hyperfine.extend(commands)

        subprocess.run(hyperfine, check=True)


if __name__ == "__main__":
    main()
