FROM rust:1.96.0 AS builder

WORKDIR /app
COPY . .

# Check that the FROM tag matches rust-toolchain.toml
RUN [ "$(rustup default | cut -d- -f1)" = "$(sed -n 's/channel = "\(.*\)"/\1/p' rust-toolchain.toml)" ] \
    || { echo "rust image tag and rust-toolchain.toml disagree"; exit 1; }

# Build the binary
RUN cargo build --release --locked

# Runtime stage: just a libc and CA certificates for TLS to the upstreams
FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 bx402

# Copy the binary from the builder stage
COPY --from=builder /app/target/release/bx402 /usr/local/bin/bx402

# Expose the port
EXPOSE 8080

# Run unprivileged: the proxy needs no root and binds 8080 (>1024)
USER bx402
ENTRYPOINT ["bx402"]
