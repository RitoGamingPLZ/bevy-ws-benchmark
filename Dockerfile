FROM rustlang/rust:nightly-slim AS builder

WORKDIR /app

# Install dependencies
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    libasound2-dev \
    libudev-dev \
    && rm -rf /var/lib/apt/lists/*


# Copy Cargo files first for better caching
COPY Cargo.toml Cargo.lock ./

# Create dummy main to cache dependencies
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release --bin bevy-server; rm -rf src

# Now copy actual source and build
COPY src ./src

# Build with faster settings
ENV CARGO_BUILD_JOBS=8
ENV CARGO_PROFILE_RELEASE_DEBUG=false
ENV CARGO_PROFILE_RELEASE_STRIP=true
ENV CARGO_PROFILE_RELEASE_LTO=false
ENV CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
ENV CARGO_PROFILE_RELEASE_OPT_LEVEL=2
RUN cargo build --release --bin bevy-server

# Runtime stage
FROM debian:bookworm-slim

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