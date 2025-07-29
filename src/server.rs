use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use clap::Parser;
use tracing::{info, warn, error};

// Configuration constants for bounded channels - optimized for 20Hz messaging with high latency
const CHANNEL_BUFFER_SIZE: usize = 100;   // ~2.5s buffer at 20Hz (handles 500ms+ delays)
const SEND_TIMEOUT_MS: u64 = 20;          // Very short timeout - let channel handle backpressure

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    bind: String,
    
    #[arg(short, long, default_value = "false")]
    stats: bool,
    
    #[arg(long, default_value = "false")]
    broadcast: bool,
    
    #[arg(long, default_value = "1")]
    broadcast_hz: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchmarkMessage {
    id: u64,
    timestamp: u64,
    payload: String,
}

#[derive(Debug, Default)]
struct ServerStats {
    connections: usize,
    total_messages: u64,
    messages_per_second: f64,
    start_time: Option<Instant>,
    last_stats_time: Option<Instant>,
    last_message_count: u64,
    dropped_messages: u64,
    slow_connections: u64,
}

type Connections = Arc<RwLock<HashMap<SocketAddr, tokio::sync::mpsc::Sender<Message>>>>;
type Stats = Arc<Mutex<ServerStats>>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    
    let args = Args::parse();
    let addr = args.bind.parse::<SocketAddr>().expect("Invalid bind address");
    
    let connections: Connections = Arc::new(RwLock::new(HashMap::new()));
    let stats: Stats = Arc::new(Mutex::new(ServerStats::default()));
    
    let listener = TcpListener::bind(&addr).await.expect("Failed to bind");
    info!("WebSocket server listening on: {}", addr);
    
    if args.stats {
        let stats_clone = stats.clone();
        let connections_clone = connections.clone();
        tokio::spawn(async move {
            print_stats_periodically(stats_clone, connections_clone).await;
        });
    }
    
    if args.broadcast {
        let connections_clone = connections.clone();
        let stats_clone = stats.clone();
        let broadcast_hz = args.broadcast_hz;
        info!("Broadcasting enabled at {}Hz", broadcast_hz);
        
        tokio::spawn(async move {
            broadcast_periodically(connections_clone, stats_clone, broadcast_hz).await;
        });
    }
    
    while let Ok((stream, addr)) = listener.accept().await {
        let connections = connections.clone();
        let stats = stats.clone();
        
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, addr, connections, stats).await {
                error!("Error handling connection {}: {}", addr, e);
            }
        });
    }
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    connections: Connections,
    stats: Stats,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_stream = accept_async(stream).await?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();
    
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(CHANNEL_BUFFER_SIZE);
    
    {
        let mut conns = connections.write().await;
        conns.insert(addr, tx);
        
        let mut stats_lock = stats.lock().await;
        stats_lock.connections = conns.len();
        if stats_lock.start_time.is_none() {
            stats_lock.start_time = Some(Instant::now());
            stats_lock.last_stats_time = Some(Instant::now());
        }
    }
    
    info!("New connection: {}", addr);
    
    let send_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            if ws_sender.send(message).await.is_err() {
                break;
            }
        }
    });
    
    let connections_clone = connections.clone();
    let stats_clone = stats.clone();
    let receive_task = tokio::spawn(async move {
        while let Some(msg) = ws_receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    {
                        let mut stats_lock = stats_clone.lock().await;
                        stats_lock.total_messages += 1;
                    }
                    
                    if let Ok(benchmark_msg) = serde_json::from_str::<BenchmarkMessage>(&text) {
                        let response = BenchmarkMessage {
                            id: benchmark_msg.id,
                            timestamp: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_millis() as u64,
                            payload: format!("echo: {}", benchmark_msg.payload),
                        };
                        
                        if let Ok(response_text) = serde_json::to_string(&response) {
                            let conns = connections_clone.read().await;
                            if let Some(sender) = conns.get(&addr) {
                                // Use bounded channel with timeout
                                match tokio::time::timeout(
                                    Duration::from_millis(SEND_TIMEOUT_MS),
                                    sender.send(Message::Text(response_text))
                                ).await {
                                    Ok(Ok(_)) => {
                                        // Message sent successfully
                                    }
                                    Ok(Err(_)) => {
                                        // Channel closed
                                        warn!("Channel closed for connection: {}", addr);
                                    }
                                    Err(_) => {
                                        // Timeout - let bounded channel handle backpressure naturally
                                        warn!("Dropped message for slow connection: {}", addr);
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Message::Binary(data)) => {
                    {
                        let mut stats_lock = stats_clone.lock().await;
                        stats_lock.total_messages += 1;
                    }
                    
                    let conns = connections_clone.read().await;
                    if let Some(sender) = conns.get(&addr) {
                        // Use bounded channel with timeout for binary messages too
                        match tokio::time::timeout(
                            Duration::from_millis(SEND_TIMEOUT_MS),
                            sender.send(Message::Binary(data))
                        ).await {
                            Ok(Ok(_)) => {
                                // Message sent successfully
                            }
                            Ok(Err(_)) => {
                                // Channel closed
                                warn!("Channel closed for connection: {}", addr);
                            }
                            Err(_) => {
                                // Timeout - let bounded channel handle backpressure naturally
                                warn!("Dropped binary message for slow connection: {}", addr);
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    warn!("WebSocket error for {}: {}", addr, e);
                    break;
                }
                _ => {}
            }
        }
    });
    
    tokio::select! {
        _ = send_task => {}
        _ = receive_task => {}
    }
    
    {
        let mut conns = connections.write().await;
        conns.remove(&addr);
        
        let mut stats_lock = stats.lock().await;
        stats_lock.connections = conns.len();
    }
    
    info!("Connection closed: {}", addr);
    Ok(())
}

async fn print_stats_periodically(stats: Stats, connections: Connections) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    
    loop {
        interval.tick().await;
        
        let (connection_count, total_messages, messages_per_second) = {
            let mut stats_lock = stats.lock().await;
            let conns = connections.read().await;
            
            let now = Instant::now();
            if let (Some(last_time), Some(_start_time)) = (stats_lock.last_stats_time, stats_lock.start_time) {
                let time_diff = now.duration_since(last_time).as_secs_f64();
                let message_diff = stats_lock.total_messages - stats_lock.last_message_count;
                
                if time_diff > 0.0 {
                    stats_lock.messages_per_second = message_diff as f64 / time_diff;
                }
            }
            
            stats_lock.last_stats_time = Some(now);
            stats_lock.last_message_count = stats_lock.total_messages;
            
            (conns.len(), stats_lock.total_messages, stats_lock.messages_per_second)
        };
        
        info!(
            "Stats - Connections: {}, Total Messages: {}, Messages/sec: {:.2}",
            connection_count, total_messages, messages_per_second
        );
    }
}

async fn broadcast_periodically(
    connections: Connections,
    stats: Stats,
    frequency_hz: u64,
) {
    let interval_ms = 1000 / frequency_hz;
    let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
    let mut broadcast_count = 0u64;
    
    loop {
        interval.tick().await;
        broadcast_count += 1;
        
        let conns = connections.read().await;
        let connection_count = conns.len();
        
        if connection_count == 0 {
            continue; // No connections to broadcast to
        }
        
        let broadcast_message = BenchmarkMessage {
            id: broadcast_count,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
            payload: format!(
                "Server broadcast #{} at {}Hz - {} connections active",
                broadcast_count, frequency_hz, connection_count
            ),
        };
        
        if let Ok(message_text) = serde_json::to_string(&broadcast_message) {
            let message = Message::Text(message_text);
            let mut successful_sends = 0;
            let mut failed_sends = 0;
            
            for (addr, sender) in conns.iter() {
                match tokio::time::timeout(
                    Duration::from_millis(SEND_TIMEOUT_MS),
                    sender.send(message.clone())
                ).await {
                    Ok(Ok(_)) => {
                        successful_sends += 1;
                    }
                    Ok(Err(_)) => {
                        warn!("Broadcast failed - channel closed for: {}", addr);
                        failed_sends += 1;
                    }
                    Err(_) => {
                        warn!("Broadcast timeout for slow connection: {}", addr);
                        failed_sends += 1;
                    }
                }
            }
            
            if broadcast_count % (frequency_hz * 10) == 0 { // Log every 10 seconds
                info!(
                    "Broadcast #{}: sent to {}/{} connections ({} failed)",
                    broadcast_count, successful_sends, connection_count, failed_sends
                );
            }
            
            // Update stats
            let mut stats_lock = stats.lock().await;
            stats_lock.total_messages += successful_sends as u64;
            stats_lock.dropped_messages += failed_sends as u64;
        }
    }
}