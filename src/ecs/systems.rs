use bevy::prelude::*;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
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
    stats: Arc<Mutex<ServerStats>>,
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
    
    let stats_start = Instant::now();
    let mut stats_lock = stats.lock().await;
    let stats_lock_time = stats_start.elapsed();
    if stats_lock_time.as_millis() > 10 {
        warn!("Slow stats lock acquisition for {}: {}ms", addr, stats_lock_time.as_millis());
    }
    
    stats_lock.connections = connections.len();
    if stats_lock.start_time.is_none() {
        stats_lock.start_time = Some(Instant::now());
        stats_lock.last_stats_time = Some(Instant::now());
    }
    drop(stats_lock); // Explicit drop to release lock quickly
    
    info!("Bevy: New WebSocket connection: {}", addr);
    
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
        
        let mut stats_lock = cleanup_stats.lock().await;
        stats_lock.connections = cleanup_connections.len();
        
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
    stats: Arc<Mutex<ServerStats>>,
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
                    
                    let response = BenchmarkMessage {
                        id: benchmark_msg.id,
                        timestamp: server_timestamp,
                        payload: format!("bevy-echo[{}]: {}", addr, benchmark_msg.payload),
                        message_type: Some("echo".to_string()),
                        original_id: Some(benchmark_msg.id),
                        server_timestamp: Some(server_timestamp),
                    };
                    let response_time = response_start.elapsed();
                    
                    let serialize_start = Instant::now();
                    if let Ok(response_text) = serde_json::to_string(&response) {
                        let serialize_time = serialize_start.elapsed();
                        
                        let send_start = Instant::now();
                        
                        // Get channel sender - scc::HashMap safe concurrent access
                        let sender_opt = connections.get(&addr).map(|entry| entry.get().clone());
                        let lock_time = send_start.elapsed();
                        
                        if let Some(sender) = sender_opt {
                            let channel_start = Instant::now();
                            match tokio::time::timeout(
                                Duration::from_millis(super::components::SEND_TIMEOUT_MS),
                                sender.send(Message::Text(response_text))
                            ).await {
                                Ok(Ok(_)) => {
                                    let channel_time = channel_start.elapsed();
                                    let total_time = start_time.elapsed();
                                    
                                    // Update stats with pending message info
                                    let pending = super::components::CHANNEL_BUFFER_SIZE - sender.capacity();
                                    {
                                        let mut stats_lock = stats.lock().await;
                                        stats_lock.total_messages += 1;
                                        stats_lock.total_pending_messages = pending;
                                        if pending > stats_lock.max_pending_messages {
                                            stats_lock.max_pending_messages = pending;
                                        }
                                    }
                                    
                                    // Log timing breakdown for slow messages (>5ms total)
                                    if total_time.as_millis() > 5 {
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
                                Ok(Err(e)) => {
                                    error!("Channel send failed for [{}]: {}", addr, e);
                                    break;
                                }
                                Err(_) => {
                                    warn!("Message timeout for slow connection [{}]: dropped after {}ms", addr, super::components::SEND_TIMEOUT_MS);
                                    let mut stats_lock = stats.lock().await;
                                    stats_lock.dropped_messages += 1;
                                    stats_lock.slow_connections += 1;
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
    runtime: Res<SharedRuntime>,
) {
    for event in disconnection_events.read() {
        runtime.runtime.block_on(async {
            server.connections.remove(&event.addr);
            
            let mut stats_lock = server.stats.lock().await;
            stats_lock.connections = server.connections.len();
        });
        
        info!("Bevy: Connection closed: {}", event.addr);
    }
}

pub fn broadcast_system(
    broadcast_config: Res<BroadcastConfig>,
    server: Res<WebSocketServer>,
    mut broadcast_events: EventWriter<BroadcastMessage>,
    runtime: Res<SharedRuntime>,
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
    runtime: Res<SharedRuntime>,
) {
    for event in broadcast_events.read() {
        runtime.runtime.block_on(async {
            broadcast_to_all_connections(&server, &event.message).await;
        });
    }
}

async fn broadcast_to_all_connections(
    server: &WebSocketServer,
    message: &str,
) {
    let broadcast_start = Instant::now();
    
    // Clone all channel senders - scc::HashMap safe iteration
    let scan_start = Instant::now();
    let mut senders: Vec<(SocketAddr, tokio::sync::mpsc::Sender<Message>)> = Vec::new();
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
            match tokio::time::timeout(
                Duration::from_millis(super::components::SEND_TIMEOUT_MS),
                sender.send(message_obj.clone())
            ).await {
                Ok(Ok(_)) => {
                    successful_sends += 1;
                }
                Ok(Err(e)) => {
                    error!("Broadcast failed - channel send error for [{}]: {}", addr, e);
                    failed_sends += 1;
                }
                Err(_) => {
                    // Timeout - channel backpressure
                    warn!("Broadcast timeout for slow connection [{}]: dropped after {}ms", addr, super::components::SEND_TIMEOUT_MS);
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
            let stats_start = Instant::now();
            let mut stats_lock = server.stats.lock().await;
            let stats_lock_time = stats_start.elapsed();
            if stats_lock_time.as_millis() > 10 {
                warn!("Slow stats lock in broadcast: {}ms", stats_lock_time.as_millis());
            }
            
            stats_lock.total_messages += successful_sends as u64;
            stats_lock.dropped_messages += failed_sends as u64;
        }
    }
}

pub fn update_stats(
    server: Res<WebSocketServer>, 
    time: Res<Time>,
    runtime: Res<SharedRuntime>,
    config: Res<ServerConfig>,
) {
    if config.stats && time.elapsed_seconds() as u64 % 5 == 0 {
        runtime.runtime.block_on(async {
            let mut stats_lock = server.stats.lock().await;
            
            let now = Instant::now();
            if let (Some(last_time), Some(_)) = (stats_lock.last_stats_time, stats_lock.start_time) {
                let time_diff = now.duration_since(last_time).as_secs_f64();
                let message_diff = stats_lock.total_messages - stats_lock.last_message_count;
                
                if time_diff > 0.0 && time_diff >= 4.5 {
                    // Get connection info and calculate pending messages - scc::HashMap safe!
                    let connection_count = server.connections.len();
                    let mut total_pending: usize = 0;
                    server.connections.scan(|_, sender| {
                        total_pending += super::components::CHANNEL_BUFFER_SIZE - sender.capacity();
                    });
                    
                    stats_lock.messages_per_second = message_diff as f64 / time_diff;
                    stats_lock.last_stats_time = Some(now);
                    stats_lock.last_message_count = stats_lock.total_messages;
                    stats_lock.total_pending_messages = total_pending;
                    if total_pending > stats_lock.max_pending_messages {
                        stats_lock.max_pending_messages = total_pending;
                    }
                    
                    info!(
                        "Bevy Stats - Connections: {}, Total Messages: {}, Messages/sec: {:.2}, Dropped: {}, Slow Clients: {}, Pending: {}, Max Pending: {}",
                        connection_count, stats_lock.total_messages, stats_lock.messages_per_second, 
                        stats_lock.dropped_messages, stats_lock.slow_connections,
                        stats_lock.total_pending_messages, stats_lock.max_pending_messages
                    );
                }
            } else {
                stats_lock.start_time = Some(now);
                stats_lock.last_stats_time = Some(now);
            }
        });
    }
}


