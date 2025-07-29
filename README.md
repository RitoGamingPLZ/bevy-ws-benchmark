# Rust WebSocket Benchmark

A WebSocket server and benchmarking suite built with Bevy ECS architecture.

## Features

- **Bevy ECS WebSocket Server** with plugin architecture
- **20Hz Broadcasting** with configurable message size for real-time applications
- **Comprehensive Benchmarking** (throughput, connections, frequency)
- **Bounded Channels** with automatic backpressure handling and monitoring
- **Message Tracing** with echo/broadcast differentiation
- **Shared Tokio Runtime** for optimal async performance
- **ARM64 Docker Support**

## Quick Start

```bash
# Build
cargo build --release

# Run server with broadcasting and stats
cargo run --bin bevy-server -- --broadcast --stats --broadcast-size 1024

# Run benchmark
cargo run --bin client -- frequency -f 20 -d 30 -c 100
```

## Parameters

### Server
- `-b, --bind` - Server bind address (default: `127.0.0.1:8080`)
- `-s, --stats` - Enable statistics with channel monitoring (default: `false`)
- `--broadcast` - Enable 20Hz broadcasting (default: `false`)
- `--broadcast-size` - Broadcast message size in bytes (default: `100`)

### Client (All modes)
- `-u, --url` - WebSocket URL (default: `ws://127.0.0.1:8080`)

### Throughput mode
- `-m, --messages` - Messages per connection (default: `1000`)
- `-p, --payload-size` - Payload size in bytes (default: `100`)
- `-c, --connections` - Concurrent connections (default: `1`)
- `-d, --delay-ms` - Message delay in ms (default: `0`)

### Connections mode
- `-x, --max-connections` - Maximum connections (default: `100`)
- `-s, --step` - Connection increment (default: `10`)
- `-h, --hold-duration` - Hold duration in seconds (default: `5`)

### Frequency mode
- `-f, --frequency` - Messages per second (default: `20`)
- `-d, --duration-seconds` - Test duration in seconds (default: `30`)
- `-p, --payload-size` - Payload size in bytes (default: `100`)
- `-c, --connections` - Concurrent connections (default: `10`)

## Examples

```bash
# Start server with broadcasting and stats
cargo run --bin bevy-server -- --broadcast --stats --broadcast-size 512

# Start server with large broadcast messages (10KB)
cargo run --bin bevy-server -- --broadcast --stats --broadcast-size 10240

# Throughput test with 10 connections, 500 messages each
cargo run --bin client -- throughput -c 10 -m 500

# Connection capacity test up to 1000 connections
cargo run --bin client -- connections -x 1000 -s 50

# Frequency test at 20Hz for 30 seconds with 100 connections
cargo run --bin client -- frequency -f 20 -d 30 -c 100
```

## Architecture

**Bevy ECS WebSocket Server** with separated async tasks:
- `src/ecs/components.rs` - Resources and Events with message tracing
- `src/ecs/systems.rs` - Connection handling and broadcasting systems
- Bounded channels (50 msg buffer, 5ms timeout) with monitoring
- Automatic backpressure handling and stats logging
- Shared Tokio runtime for optimal async performance

### Message Types
- **Echo messages**: Include `message_type: "echo"`, original message ID, and client address
- **Broadcast messages**: Include `message_type: "broadcast"`, unique timestamp ID, and configurable payload size
- **Channel monitoring**: Real-time pending message counts and peak usage tracking

## Docker

```bash
docker build --platform linux/arm64 -t rust-ws-benchmark .
docker run -p 8080:8080 rust-ws-benchmark
```

