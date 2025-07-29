FROM --platform=linux/arm64 rust:1.75-slim as builder

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

# Build the server binary for ARM64
RUN cargo build --release --bin server

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
COPY --from=builder /app/target/release/server ./server

# Change ownership to non-root user
RUN chown wsserver:wsserver ./server

# Switch to non-root user
USER wsserver

# Expose the default port
EXPOSE 8080

# Set default command
CMD ["./server", "--bind", "0.0.0.0:8080", "--stats"]