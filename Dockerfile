FROM rust:1-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN cargo build -p rpx --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git openssh-client \
    && rm -rf /var/lib/apt/lists/*

COPY --link --from=builder /app/target/release/rpx /usr/local/bin/rpx
RUN cp /usr/local/bin/rpx /rpx

ENTRYPOINT ["rpx"]
CMD ["--help"]

FROM r-base:latest AS test

RUN apt-get update \
    && apt-get install -y --no-install-recommends git openssh-client \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/rpx /usr/local/bin/rpx
CMD ["sleep", "infinity"]
