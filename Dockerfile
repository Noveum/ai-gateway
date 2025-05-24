# Build stage
FROM rust:1.82-slim-bookworm AS builder

# Install required dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Create a new empty shell project
WORKDIR /usr/src/app

# Copy only necessary files first
COPY Cargo.toml Cargo.lock ./

# Create dummy source files to build dependencies
RUN mkdir src && \
    echo "fn main() {}" > src/main.rs && \
    echo "// dummy lib file" > src/lib.rs && \
    cargo build --release --target x86_64-unknown-linux-gnu && \
    rm -rf src

# Now copy the real source code
COPY src ./src

# Build the application
RUN RUSTFLAGS='-C target-feature=+crt-static' cargo build --release --target x86_64-unknown-linux-gnu && \
    strip target/x86_64-unknown-linux-gnu/release/noveum-ai-gateway

# Runtime stage
FROM debian:bookworm-slim

# Add LABEL to identify the image
LABEL org.opencontainers.image.source="https://github.com/noveum/ai-gateway"
LABEL org.opencontainers.image.description="Noveum AI Gateway"
LABEL org.opencontainers.image.version="latest"

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy the binary from builder
COPY --from=builder /usr/src/app/target/x86_64-unknown-linux-gnu/release/noveum-ai-gateway /usr/local/bin/

# Set the startup command
CMD ["noveum-ai-gateway"] 