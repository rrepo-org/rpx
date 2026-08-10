#!/usr/bin/env python3

import argparse
import os
import shlex
import shutil
import subprocess
import tempfile
from pathlib import Path


BENCHMARK_DIR = Path(__file__).resolve().parent
FIXTURES_DIR = BENCHMARK_DIR / "fixtures"
CACHE_MODES = ("cold", "warm")
FIXTURE_LABELS = {
    "digest": "Digest",
    "rlang-1.0.1": "rlang 1.0.1",
    "tidyverse-current": "Tidyverse, current",
    "vctrs-0.3.8": "vctrs 0.3.8",
}


def fixture_names() -> list[str]:
    return sorted(
        path.name
        for path in FIXTURES_DIR.iterdir()
        if path.is_dir() and (path / "DESCRIPTION").is_file()
    )


def parse_args() -> argparse.Namespace:
    fixtures = fixture_names()
    parser = argparse.ArgumentParser(
        description="Benchmark rpx resolution against the live registry."
    )
    parser.add_argument(
        "--build-release",
        action="store_true",
        help="Build and benchmark target/release/rpx instead of the installed rpx.",
    )
    parser.add_argument(
        "--fixture",
        action="append",
        choices=fixtures,
        help="Fixture to benchmark; may be repeated (default: all fixtures).",
    )
    parser.add_argument(
        "--cache",
        action="append",
        choices=CACHE_MODES,
        help="Cache mode to benchmark; may be repeated (default: cold and warm).",
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
        "--output-dir",
        type=Path,
        default=BENCHMARK_DIR / "results",
        help="Directory for Hyperfine JSON output (default: benchmarks/results).",
    )
    return parser.parse_args()


def run_benchmarks(
    fixture_names: list[str],
    cache_modes: list[str],
    binary: Path,
    warmup: int,
    runs: int,
    output_dir: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="rpx-benchmark-") as temporary_dir:
        temporary_dir = Path(temporary_dir)
        names = []
        prepares = []
        commands = []

        for fixture_name in fixture_names:
            fixture = FIXTURES_DIR / fixture_name
            for cache_mode in cache_modes:
                project_dir = temporary_dir / f"{fixture_name}-{cache_mode}"
                shutil.copytree(fixture, project_dir)
                remove_lockfile = (
                    f"rm -f {shlex.quote(str(project_dir / 'rpx.lock'))}"
                )
                if cache_mode == "cold":
                    clean = (
                        f"cd {shlex.quote(str(project_dir))} && "
                        f"{shlex.quote(str(binary))} clean >/dev/null 2>&1"
                    )
                    prepare = f"{clean} && {remove_lockfile}"
                else:
                    prepare = remove_lockfile
                command = (
                    f"cd {shlex.quote(str(project_dir))} && "
                    f"{shlex.quote(str(binary))} lock"
                )

                names.append(f"{FIXTURE_LABELS[fixture_name]} ({cache_mode} cache)")
                prepares.append(prepare)
                commands.append(command)

            if "warm" in cache_modes and "cold" not in cache_modes:
                prime_dir = temporary_dir / f"{fixture_name}-prime"
                shutil.copytree(fixture, prime_dir)
                subprocess.run(
                    [str(binary), "lock"],
                    cwd=prime_dir,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=True,
                )

        print(
            f"Benchmarking {len(commands)} scenarios with {binary}",
            flush=True,
        )
        hyperfine = [
            "hyperfine",
            "--warmup",
            str(warmup),
            "--runs",
            str(runs),
        ]
        hyperfine.extend(["--export-json", str(output_dir / "benchmarks.json")])
        for name in names:
            hyperfine.extend(["--command-name", name])
        for prepare in prepares:
            hyperfine.extend(["--prepare", prepare])
        hyperfine.extend(commands)

        subprocess.run(hyperfine, check=True)


def select_binary(build_release: bool) -> Path:
    if build_release:
        subprocess.run(
            ["cargo", "build", "--release"],
            cwd=BENCHMARK_DIR.parent,
            check=True,
        )
        path = BENCHMARK_DIR.parent / "target" / "release" / "rpx"
    else:
        installed = shutil.which("rpx")
        if installed is None:
            raise SystemExit("installed rpx was not found on PATH")
        path = Path(installed)

    path = path.resolve()
    if not path.is_file():
        raise SystemExit(f"rpx binary does not exist: {path}")
    if not os.access(path, os.X_OK):
        raise SystemExit(f"rpx binary is not executable: {path}")
    return path


def main() -> None:
    args = parse_args()
    if args.warmup < 0:
        raise SystemExit("--warmup must be at least 0")
    if args.runs < 1:
        raise SystemExit("--runs must be at least 1")
    if shutil.which("hyperfine") is None:
        raise SystemExit("hyperfine is required but was not found on PATH")

    binary = select_binary(args.build_release)

    output_dir = args.output_dir.expanduser().resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    fixtures = list(dict.fromkeys(args.fixture or fixture_names()))
    selected_cache_modes = set(args.cache or CACHE_MODES)
    cache_modes = [mode for mode in CACHE_MODES if mode in selected_cache_modes]
    run_benchmarks(
        fixtures,
        cache_modes,
        binary,
        args.warmup,
        args.runs,
        output_dir,
    )


if __name__ == "__main__":
    main()
