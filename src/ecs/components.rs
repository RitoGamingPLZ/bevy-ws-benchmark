use bevy::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;
use tokio::net::TcpListener;
use serde::{Deserialize, Serialize};
use scc::HashMap as SccHashMap;

// Configuration constants for bounded channels - optimized for 20Hz messaging with high latency
pub const CHANNEL_BUFFER_SIZE: usize = 50;   // ~2.5s buffer at 20Hz (handles 500ms+ delays)
pub const SEND_TIMEOUT_MS: u64 = 5;          // Very short timeout - let channel handle backpressure

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkMessage {
    pub id: u64,
    pub timestamp: u64,
    pub payload: String,
    #[serde(default)]
    pub message_type: Option<String>,
    #[serde(default)]
    pub original_id: Option<u64>,
    #[serde(default)]
    pub server_timestamp: Option<u64>,
}

#[derive(Resource, Debug, Default)]
pub struct ServerStats {
    pub connections: usize,
    pub total_messages: u64,
    pub messages_per_second: f64,
    pub start_time: Option<Instant>,
    pub last_stats_time: Option<Instant>,
    pub last_message_count: u64,
    pub dropped_messages: u64,
    pub slow_connections: u64,
    pub total_pending_messages: usize,
    pub max_pending_messages: usize,
}

#[derive(Resource)]
pub struct WebSocketServer {
    pub connections: Arc<SccHashMap<SocketAddr, tokio::sync::mpsc::Sender<Message>>>,
    pub stats: Arc<Mutex<ServerStats>>,
    pub listener: Option<TcpListener>,
}

impl WebSocketServer {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(SccHashMap::new()),
            stats: Arc::new(Mutex::new(ServerStats::default())),
            listener: None,
        }
    }
}

#[derive(Event)]
pub struct MessageReceived {
    pub from: SocketAddr,
    pub message: String,
}

#[derive(Event)]
pub struct ConnectionClosed {
    pub addr: SocketAddr,
}

#[derive(Event)]
pub struct BroadcastMessage {
    pub message: String,
}

#[derive(Resource)]
pub struct BroadcastConfig {
    pub enabled: bool,
}

#[derive(Resource)]
pub struct ServerConfig {
    pub bind: String,
    pub stats: bool,
    pub broadcast: bool,
    pub broadcast_size: usize,
}

#[derive(Resource)]
pub struct SharedRuntime {
    pub runtime: Arc<tokio::runtime::Runtime>,
}

impl SharedRuntime {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("Failed to create Tokio runtime");
        Self {
            runtime: Arc::new(runtime),
        }
    }
}

