use bevy::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use tokio::net::TcpListener;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, AtomicU64};
use scc::HashMap as SccHashMap;

// Configuration constants for bounded channels - optimized for low latency
pub const CHANNEL_BUFFER_SIZE: usize = 200;  // Larger buffer to reduce contention

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

#[derive(Resource, Debug)]
pub struct ServerStats {
    pub connections: AtomicUsize,
    pub total_messages: AtomicU64,
    pub dropped_messages: AtomicU64,
    pub slow_connections: AtomicU64,
    pub total_pending_messages: AtomicUsize,
    pub max_pending_messages: AtomicUsize,
    // Fields below are updated/read only in update_stats system:
    pub start_time: Option<Instant>,
    pub last_stats_time: Option<Instant>,
    pub messages_per_second: f64,
    pub last_message_count: u64,
}

impl Default for ServerStats {
    fn default() -> Self {
        Self {
            connections: AtomicUsize::new(0),
            total_messages: AtomicU64::new(0),
            dropped_messages: AtomicU64::new(0),
            slow_connections: AtomicU64::new(0),
            total_pending_messages: AtomicUsize::new(0),
            max_pending_messages: AtomicUsize::new(0),
            start_time: None,
            last_stats_time: None,
            messages_per_second: 0.0,
            last_message_count: 0,
        }
    }
}

#[derive(Resource)]
pub struct WebSocketServer {
    pub connections: Arc<SccHashMap<SocketAddr, tokio::sync::mpsc::Sender<Message>>>,
    pub stats: Arc<ServerStats>,
    pub listener: Option<TcpListener>,
}

impl WebSocketServer {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(SccHashMap::new()),
            stats: Arc::new(ServerStats::default()),
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

