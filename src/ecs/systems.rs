use bevy::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{Ordering};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use clap::Parser;
use tracing::{info, warn, error};
use scc::HashMap as SccHashMap;

use super::components::*;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(author, version, about)]
pub struct Args {
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    pub bind: String,
    
    #[arg(short, long, default_value = "false")]
    pub stats: bool,
    
    #[arg(long, default_value = "false")]
    pub broadcast: bool,
    
    #[arg(long, default_value = "100")]
    pub broadcast_size: usize,
}

pub fn setup_websocket_server(
    mut server: ResMut<WebSocketServer>,
    config: Res<ServerConfig>,
    runtime: Res<SharedRuntime>,
) {
    let addr = config.bind.parse::<SocketAddr>().expect("Invalid bind address");
    
    let listener = runtime.runtime.block_on(async {
        TcpListener::bind(&addr).await.expect("Failed to bind")
    });
    
    info!("Bevy WebSocket server listening on: {}", addr);
    server.listener = Some(listener);
    
    if config.broadcast {
        info!("Broadcasting enabled at 20Hz via FixedUpdate");
    }
    
}

pub fn handle_new_connections(
    server: Res<WebSocketServer>,
    runtime: Res<SharedRuntime>,
) {
    if let Some(listener) = &server.listener {
        let accept_start = Instant::now();
        match runtime.runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(1), listener.accept()).await
        }) {
            Ok(Ok((stream, addr))) => {
                let accept_time = accept_start.elapsed();
                info!("TCP connection accepted from: {} in {}μs", addr, accept_time.as_micros());
                if accept_time.as_millis() > 10 {
                    warn!("Slow TCP accept for {}: {}ms", addr, accept_time.as_millis());
                }
                
                let connections = server.connections.clone();
                let stats = server.stats.clone();
                
                // Spawn WebSocket setup task on shared runtime (fire and forget)
                let spawn_start = Instant::now();
                runtime.runtime.spawn(async move {
                    if let Err(e) = setup_websocket_connection(stream, addr, connections, stats).await {
                        error!("WebSocket setup failed for {}: {}", addr, e);
                    }
                });
                let spawn_time = spawn_start.elapsed();
                if spawn_time.as_millis() > 10 {
                    warn!("Slow runtime spawn for {}: {}ms", addr, spawn_time.as_millis());
                }
            }
            Ok(Err(e)) => {
                error!("TCP accept error: {}", e);
            }
            Err(_) => {
                // Timeout, no new connections available (this is normal)
            }
        }
    }
}

async fn setup_websocket_connection(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    connections: Arc<SccHashMap<SocketAddr, tokio::sync::mpsc::Sender<Message>>>,
    stats: Arc<ServerStats>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let setup_start = Instant::now();
    info!("Starting WebSocket setup for: {}", addr);
    
    // WebSocket handshake with timeout
    let handshake_start = Instant::now();
    let ws_stream = match tokio::time::timeout(Duration::from_secs(10), accept_async(stream)).await {
        Ok(Ok(stream)) => {
            let handshake_time = handshake_start.elapsed();
            info!("WebSocket handshake completed for: {} in {}ms", addr, handshake_time.as_millis());
            if handshake_time.as_millis() > 100 {
                warn!("Slow WebSocket handshake for {}: {}ms", addr, handshake_time.as_millis());
            }
            stream
        }
        Ok(Err(e)) => {
            error!("WebSocket handshake failed for {}: {} (after {}ms)", addr, e, handshake_start.elapsed().as_millis());
            return Err(e.into());
        }
        Err(_) => {
            error!("WebSocket handshake timeout for {} (after 10s)", addr);
            return Err("WebSocket handshake timeout".into());
        }
    };
    
    let split_start = Instant::now();
    let (ws_sender, ws_receiver) = ws_stream.split();
    let split_time = split_start.elapsed();
    if split_time.as_millis() > 10 {
        warn!("Slow WebSocket split for {}: {}ms", addr, split_time.as_millis());
    }
    
    // Create bounded channel for this connection
    let channel_start = Instant::now();
    let (tx, rx) = tokio::sync::mpsc::channel::<Message>(super::components::CHANNEL_BUFFER_SIZE);
    let channel_time = channel_start.elapsed();
    if channel_time.as_millis() > 10 {
        warn!("Slow channel creation for {}: {}ms", addr, channel_time.as_millis());
    }
    
    // Register connection - scc::HashMap provides safe concurrent access
    let insert_start = Instant::now();
    let insert_result = connections.insert(addr, tx);
    let insert_time = insert_start.elapsed();
    if insert_result.is_err() {
        error!("Failed to insert connection [{}] into connections map", addr);
    }
    if insert_time.as_millis() > 10 {
        warn!("Slow connection insert for {}: {}ms", addr, insert_time.as_millis());
    }
    
    stats.connections.store(connections.len(), Ordering::Relaxed);
    
    // info!("Bevy: New WebSocket connection: {}", addr);
    
    // Spawn sender task (channel → WebSocket)
    let spawn_start = Instant::now();
    let send_task = tokio::spawn(websocket_sender_task(ws_sender, rx));
    
    // Spawn receiver task (WebSocket → Message processing)
    let receive_task = tokio::spawn(websocket_receiver_task(
        ws_receiver, 
        addr, 
        connections.clone(), 
        stats.clone()
    ));
    
    let spawn_time = spawn_start.elapsed();
    if spawn_time.as_millis() > 10 {
        warn!("Slow task spawning for {}: {}ms", addr, spawn_time.as_millis());
    }
    
    let total_setup_time = setup_start.elapsed();
    info!("WebSocket tasks spawned for: {} (total setup: {}ms)", addr, total_setup_time.as_millis());
    if total_setup_time.as_millis() > 50 {
        warn!("Slow total connection setup for {}: {}ms", addr, total_setup_time.as_millis());
    }
    
    // Don't wait for tasks to complete - let them run independently
    // Cleanup will be handled by the receive task when it detects disconnection
    let cleanup_connections = connections.clone();
    let cleanup_stats = stats.clone();
    tokio::spawn(async move {
        // Wait for either task to complete then cleanup
        tokio::select! {
            result = send_task => {
                info!("Send task completed for {}: {:?}", addr, result);
            }
            result = receive_task => {
                info!("Receive task completed for {}: {:?}", addr, result);
            }
        }
        
        // Clean up connection - scc::HashMap safe removal
        cleanup_connections.remove(&addr);
        cleanup_stats.connections.store(cleanup_connections.len(), Ordering::Relaxed);
        info!("Bevy: WebSocket connection closed: {}", addr);
    });
    
    Ok(())
}

async fn websocket_sender_task(
    mut ws_sender: futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, Message>,
    mut rx: tokio::sync::mpsc::Receiver<Message>,
) {
    while let Some(message) = rx.recv().await {
        if let Err(e) = ws_sender.send(message).await {
            error!("WebSocket send error in sender task: {}", e);
            break;
        }
    }
    info!("WebSocket sender task terminated");
}

async fn websocket_receiver_task(
    mut ws_receiver: futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>>,
    addr: SocketAddr,
    connections: Arc<SccHashMap<SocketAddr, tokio::sync::mpsc::Sender<Message>>>,
    stats: Arc<ServerStats>,
) {
    while let Some(msg) = ws_receiver.next().await {
        let start_time = Instant::now();
        
        match msg {
            Ok(Message::Text(text)) => {
                let parse_start = Instant::now();
                
                // Handle message processing directly (like the original monolithic approach)
                match serde_json::from_str::<BenchmarkMessage>(&text) {
                    Ok(benchmark_msg) => {
                        let parse_time = parse_start.elapsed();
                    
                    let response_start = Instant::now();
                    let server_timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    
                    // Pre-allocate payload string to reduce allocations
                    let mut payload = String::with_capacity(benchmark_msg.payload.len() + 50);
                    payload.push_str("bevy-echo[");
                    payload.push_str(&addr.to_string());
                    payload.push_str("]: ");
                    payload.push_str(&benchmark_msg.payload);
                    
                    let response = BenchmarkMessage {
                        id: benchmark_msg.id,
                        timestamp: server_timestamp,
                        payload,
                        message_type: Some("echo".to_string()),
                        original_id: Some(benchmark_msg.id),
                        server_timestamp: Some(server_timestamp),
                    };
                    let response_time = response_start.elapsed();
                    
                    let serialize_start = Instant::now();
                    if let Ok(response_text) = serde_json::to_string(&response) {
                        let serialize_time = serialize_start.elapsed();
                        
                        let send_start = Instant::now();
                        
                        // Direct access to sender using scc::HashMap optimized lookup
                        if let Some(entry) = connections.get(&addr) {
                            let sender = entry.get().clone();
                            let lock_time = send_start.elapsed();
                            let channel_start = Instant::now();
                            // Use try_send for instant, non-blocking operation
                            match sender.try_send(Message::Text(response_text)) {
                                Ok(_) => {
                                    let channel_time = channel_start.elapsed();
                                    let total_time = start_time.elapsed();
                                    
                                    // Update stats with pending message info
                                    let pending = super::components::CHANNEL_BUFFER_SIZE - sender.capacity();
                                    stats.total_messages.fetch_add(1, Ordering::Relaxed);
                                    stats.total_pending_messages.store(pending, Ordering::Relaxed);
                                    let prev_max = stats.max_pending_messages.load(Ordering::Relaxed);
                                    if pending > prev_max {
                                        stats.max_pending_messages.store(pending, Ordering::Relaxed);
                                    }
                                    
                                    // Log timing breakdown for slow messages (>2ms total, reduced threshold)
                                    if total_time.as_millis() > 10 {
                                        warn!(
                                            "Slow message processing [{}]: parse={}μs, response={}μs, serialize={}μs, lock={}μs, channel={}μs, total={}ms",
                                            addr, 
                                            parse_time.as_micros(),
                                            response_time.as_micros(), 
                                            serialize_time.as_micros(),
                                            lock_time.as_micros(),
                                            channel_time.as_micros(),
                                            total_time.as_millis()
                                        );
                                    }
                                }
                                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                    // Channel full - drop message immediately instead of waiting
                                    warn!("Channel full for connection [{}]: message dropped instantly", addr);
                                    stats.dropped_messages.fetch_add(1, Ordering::Relaxed);
                                    stats.slow_connections.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                    error!("Channel closed for [{}]: connection terminated", addr);
                                    break;
                                }
                            }
                        } else {
                            warn!("Connection [{}] not found in connections map", addr);
                        }
                    } else {
                        error!("Failed to serialize response message for [{}]", addr);
                    }
                    }
                    Err(e) => {
                        error!("JSON parse error for [{}]: {} - Message: '{}'", addr, e, text);
                    }
                }
            }
            Ok(Message::Close(_)) => break,
            Err(e) => {
                error!("WebSocket receive error for [{}]: {}", addr, e);
                break;
            }
            _ => {}
        }
    }
}



pub fn handle_disconnections(
    mut disconnection_events: EventReader<ConnectionClosed>,
    server: Res<WebSocketServer>,
    _runtime: Res<SharedRuntime>,
) {
    for event in disconnection_events.read() {
        _runtime.runtime.block_on(async {
            server.connections.remove(&event.addr);
            server.stats.connections.store(server.connections.len(), Ordering::Relaxed);
        });
        
        // info!("Bevy: Connection closed: {}", event.addr);
    }
}

pub fn broadcast_system(
    broadcast_config: Res<BroadcastConfig>,
    server: Res<WebSocketServer>,
    mut broadcast_events: EventWriter<BroadcastMessage>,
    _runtime: Res<SharedRuntime>,
    config: Res<ServerConfig>,
) {
    if !broadcast_config.enabled {
        return;
    }
    
    // This system runs at exactly 20Hz via FixedUpdate - no timing logic needed!
    let connection_count = server.connections.len();
    
    // Skip broadcasting if no connections
    if connection_count == 0 {
        return;
    }
    
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    
    // Create base message
    let base_message = format!(
        "Server broadcast at {}ms - {} connections active - 20Hz FixedUpdate",
        timestamp, connection_count
    );
    
    // Add dummy padding to reach desired size
    let current_size = base_message.len();
    let dummy_data = if config.broadcast_size > current_size {
        let padding_size = config.broadcast_size - current_size;
        format!(" [DUMMY:{}]", "X".repeat(padding_size.saturating_sub(10)))
    } else {
        String::new()
    };
    
    let message = format!("{}{}", base_message, dummy_data);
        
    broadcast_events.send(BroadcastMessage {
        message,
    });
}

pub fn handle_broadcast_messages(
    mut broadcast_events: EventReader<BroadcastMessage>,
    server: Res<WebSocketServer>,
    _runtime: Res<SharedRuntime>,
) {
    for event in broadcast_events.read() {
        _runtime.runtime.block_on(async {
            broadcast_to_all_connections(&server, &event.message).await;
        });
    }
}

async fn broadcast_to_all_connections(
    server: &WebSocketServer,
    message: &str,
) {
    let broadcast_start = Instant::now();
    
    // Clone all channel senders - scc::HashMap safe iteration with pre-allocated capacity
    let scan_start = Instant::now();
    let estimated_connections = server.connections.len();
    let mut senders: Vec<(SocketAddr, tokio::sync::mpsc::Sender<Message>)> = Vec::with_capacity(estimated_connections);
    server.connections.scan(|addr, sender| {
        senders.push((*addr, sender.clone()));
    });
    let scan_time = scan_start.elapsed();
    
    let mut successful_sends = 0;
    let mut failed_sends = 0;
    let total_connections = senders.len();
    
    if scan_time.as_millis() > 10 {
        warn!("Slow connection scan for broadcast: {}ms ({} connections)", scan_time.as_millis(), total_connections);
    }
    
    let server_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    
    let broadcast_message = BenchmarkMessage {
        id: server_time, // Use timestamp as unique broadcast ID
        timestamp: server_time,
        payload: format!("broadcast[{}]: {}", server_time, message),
        message_type: Some("broadcast".to_string()),
        original_id: None,
        server_timestamp: Some(server_time),
    };
    
    if let Ok(message_text) = serde_json::to_string(&broadcast_message) {
        let message_obj = Message::Text(message_text);
        
        // Send to all connections via channels without holding the connections lock
        for (addr, sender) in senders {
            // Use try_send for instant, non-blocking broadcast
            match sender.try_send(message_obj.clone()) {
                Ok(_) => {
                    successful_sends += 1;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // Channel full - drop broadcast message immediately
                    warn!("Broadcast dropped for slow connection [{}]: channel full", addr);
                    failed_sends += 1;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Connection closed - this is normal
                    failed_sends += 1;
                }
            }
        }
        
        if total_connections > 0 {
            let total_broadcast_time = broadcast_start.elapsed();
            info!(
                "Broadcast sent to {}/{} connections ({} failed) in {}ms", 
                successful_sends, total_connections, failed_sends, total_broadcast_time.as_millis()
            );
            
            if total_broadcast_time.as_millis() > 100 {
                warn!("Slow broadcast: {}ms for {} connections", total_broadcast_time.as_millis(), total_connections);
            }
            
            // Update stats
            server.stats.total_messages.fetch_add(successful_sends as u64, Ordering::Relaxed);
            server.stats.dropped_messages.fetch_add(failed_sends as u64, Ordering::Relaxed);
        }
    }
}

pub fn update_stats(
    server: ResMut<WebSocketServer>, 
    time: Res<Time>,
    config: Res<ServerConfig>,
) {
    if config.stats && time.elapsed_seconds() as u64 % 5 == 0 {
        let stats = &*server.stats;
        let now = Instant::now();
        
        // Initialize timing if needed (unsafe access to non-atomic fields)
        let stats_ptr = Arc::as_ptr(&server.stats) as *mut ServerStats;
        unsafe {
            if (*stats_ptr).start_time.is_none() {
                (*stats_ptr).start_time = Some(now);
                (*stats_ptr).last_stats_time = Some(now);
                return;
            }
            
            if let Some(last_time) = (*stats_ptr).last_stats_time {
                let time_diff = now.duration_since(last_time).as_secs_f64();
                
                if time_diff >= 4.5 {
                    let current_messages = stats.total_messages.load(Ordering::Relaxed);
                    let message_diff = current_messages - (*stats_ptr).last_message_count;
                    
                    // Get connection info and calculate pending messages
                    let connection_count = server.connections.len();
                    let mut total_pending: usize = 0;
                    server.connections.scan(|_, sender| {
                        total_pending += super::components::CHANNEL_BUFFER_SIZE - sender.capacity();
                    });
                    
                    // Update non-atomic fields
                    (*stats_ptr).messages_per_second = message_diff as f64 / time_diff;
                    (*stats_ptr).last_stats_time = Some(now);
                    (*stats_ptr).last_message_count = current_messages;
                    
                    // Update atomic pending stats
                    stats.total_pending_messages.store(total_pending, Ordering::Relaxed);
                    let prev_max = stats.max_pending_messages.load(Ordering::Relaxed);
                    if total_pending > prev_max {
                        stats.max_pending_messages.store(total_pending, Ordering::Relaxed);
                    }
                    
                    info!(
                        "Bevy Stats - Connections: {}, Total Messages: {}, Messages/sec: {:.2}, Dropped: {}, Slow Clients: {}, Pending: {}, Max Pending: {}",
                        connection_count, 
                        current_messages, 
                        (*stats_ptr).messages_per_second,
                        stats.dropped_messages.load(Ordering::Relaxed),
                        stats.slow_connections.load(Ordering::Relaxed),
                        total_pending,
                        stats.max_pending_messages.load(Ordering::Relaxed)
                    );
                }
            }
        }
    }
}


