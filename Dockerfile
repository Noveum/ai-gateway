# Build stage
# Rust 1.96 (>= the crate's MSRV of 1.94.1); 1.82 is too old — some transitive
# dependencies now ship `edition = "2024"` manifests that need Cargo >= 1.85.
FROM --platform=linux/amd64 rust:1.96-slim-bookworm AS builder

# Install required dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create a new empty shell project
WORKDIR /usr/src/app

# Copy only necessary files first
COPY Cargo.toml Cargo.lock ./

# Create dummy sources to pre-build dependencies. The crate now exposes BOTH a
# library target (the Cloudflare Worker / Rust-library shape — see [lib] in
# Cargo.toml) and the binary, so cargo needs src/lib.rs to exist here too.
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "" > src/lib.rs && \
    cargo build --release --target x86_64-unknown-linux-gnu && \
    rm -rf src

# Now copy the real source code, plus the data directories the crate embeds at
# compile time via include_str! — without them cargo cannot compile the lib:
#   schema/  -> src/policy/policy_types.rs (policy JSON Schema)
#   pricing/ -> src/policy/pricing.rs (catalog integrity check; test-only today,
#               copied anyway so a future non-test embed can't break only Docker)
COPY src ./src
COPY schema ./schema
COPY pricing ./pricing

# Build the application
RUN RUSTFLAGS='-C target-feature=+crt-static' cargo build --release --target x86_64-unknown-linux-gnu && \
    strip target/x86_64-unknown-linux-gnu/release/noveum-ai-gateway

# Runtime stage
FROM --platform=linux/amd64 debian:bookworm-slim

# Add LABEL to identify the image
ARG VERSION=dev
ARG REVISION=unknown
LABEL org.opencontainers.image.source="https://github.com/noveum/ai-gateway"
LABEL org.opencontainers.image.description="Noveum AI Gateway"
LABEL org.opencontainers.image.version=$VERSION
LABEL org.opencontainers.image.revision=$REVISION

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /usr/src/app/target/x86_64-unknown-linux-gnu/release/noveum-ai-gateway /usr/local/bin/

# Native installations default to loopback for safety. Containers must listen
# on every container interface so published ports and orchestrator probes can
# reach the process; operators can still override HOST at runtime.
ENV HOST=0.0.0.0

# The gateway needs no filesystem writes or privileged port. A fixed numeric
# identity also works in minimal images without requiring passwd/group entries.
# Any mounted policy or configuration files must be readable by this identity.
USER 65532:65532

# Set the startup command
CMD ["noveum-ai-gateway"]
