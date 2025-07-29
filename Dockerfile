FROM --platform=linux/arm64 rustlang/rust:nightly-slim as builder

WORKDIR /app

# Install dependencies for cross-compilation and linking
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Copy manifest files
COPY Cargo.toml ./

# Copy source code
COPY src/ ./src/

# Build the bevy-server binary for ARM64
RUN cargo build --release --bin bevy-server

# Runtime stage
FROM --platform=linux/arm64 debian:bookworm-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user
RUN useradd -r -s /bin/false wsserver

WORKDIR /app

# Copy the binary from builder stage
COPY --from=builder /app/target/release/bevy-server ./bevy-server

# Change ownership to non-root user
RUN chown wsserver:wsserver ./bevy-server

# Switch to non-root user
USER wsserver

# Set environment variables
ENV RUST_LOG=info

# Expose the default port
EXPOSE 8080

# Set default command
CMD ["./bevy-server", "--bind", "0.0.0.0:8080", "--stats", "--broadcast", "--broadcast-size", "400"]