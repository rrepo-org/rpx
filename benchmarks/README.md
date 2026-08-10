# Benchmarks

This directory contains end-to-end dependency-resolution benchmarks against the
live registry. Each benchmark runs the complete `rpx lock` command in an
isolated temporary project.

The suite includes four fixtures:

- `digest`: shallow resolution and fixed-overhead control
- `tidyverse-current`: a broad current dependency graph
- `rlang-1.0.1`: successful historical backtracking
- `vctrs-0.3.8`: heavier historical backtracking

Each fixture runs in two cache modes. Cold runs invoke `rpx clean` before every
timed lock. Warm runs perform one untimed lock first, then retain persistent rpx
cache state while removing `rpx.lock` before every timed lock. Repository
metadata caches are process-local, so they start empty in both modes.

## Requirements

- A working R installation
- [uv](https://docs.astral.sh/uv/)
- [Hyperfine](https://github.com/sharkdp/hyperfine)
- An installed release build of `rpx`, or Cargo to build the current checkout

## Run

Install the revision to benchmark as the baseline:

```shell
git checkout <baseline-revision>
cargo install --path . --force
```

Return to the benchmark suite and run the installed binary:

```shell
git checkout <benchmark-revision>
uv run --project benchmarks --locked python benchmarks/benchmark.py \
  --output-dir benchmarks/results/baseline
```

To benchmark the current checkout instead, build a fresh release before the
suite starts:

```shell
uv run --project benchmarks --locked python benchmarks/benchmark.py \
  --build-release \
  --output-dir benchmarks/results/current
```

With no filters, this runs all four fixtures in both cold and warm modes. Use
repeatable `--fixture` and `--cache` options to run a subset:

```shell
uv run --project benchmarks --locked python benchmarks/benchmark.py \
  --fixture rlang-1.0.1 \
  --cache cold
```

The harness passes all selected scenarios to one Hyperfine invocation, with
names such as `rlang 1.0.1 (cold cache)`. It defaults to one warmup and five
measured runs. Both values can be overridden, and the combined results can be
exported to `benchmarks.json` in the selected output directory. Results default
to `benchmarks/results/benchmarks.json`:

```shell
uv run --project benchmarks --locked python benchmarks/benchmark.py \
  --build-release \
  --warmup 3 \
  --runs 10 \
  --output-dir benchmarks/results
```
