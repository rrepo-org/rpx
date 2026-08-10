# Benchmarks

This directory contains a minimal end-to-end benchmark for fresh dependency
resolution against the live registry. It uses the published `rlang 1.0.1`
DESCRIPTION as the project fixture and removes `rpx.lock` before every run.

## Requirements

- A working R installation
- [uv](https://docs.astral.sh/uv/)
- [Hyperfine](https://github.com/sharkdp/hyperfine)
- One or more release builds of `rpx`

## Run

Build and retain a baseline binary, then build the version to compare:

```shell
cargo build --release
cp target/release/rpx /tmp/rpx-baseline
# Make or check out the changes to benchmark.
cargo build --release
```

Run both binaries in the same Hyperfine comparison:

```shell
uv run --project benchmarks --locked python benchmarks/benchmark.py \
  --rpx-path /tmp/rpx-baseline \
  --rpx-path target/release/rpx
```

The harness defaults to one warmup and five measured runs. Both values can be
overridden, and results can be exported as JSON:

```shell
uv run --project benchmarks --locked python benchmarks/benchmark.py \
  --rpx-path target/release/rpx \
  --warmup 3 \
  --runs 10 \
  --export-json results.json
```
