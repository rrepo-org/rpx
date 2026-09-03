FROM rust:1 AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY assets ./assets
COPY src ./src
RUN cargo build

FROM r-base:latest
RUN apt-get update \
    && apt-get install -y --no-install-recommends git openssh-client \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/debug/rpx /usr/local/bin/rpx
RUN chmod +x /usr/local/bin/rpx
CMD ["sleep", "infinity"]
