use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use clap::{Parser, Subcommand};
use tracing::{info, warn, error};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Throughput {
        #[arg(short, long, default_value = "ws://127.0.0.1:8080")]
        url: String,
        
        #[arg(short, long, default_value = "1000")]
        messages: u64,
        
        #[arg(short, long, default_value = "100")]
        payload_size: usize,
        
        #[arg(short, long, default_value = "1")]
        connections: usize,
        
        #[arg(short, long, default_value = "0")]
        delay_ms: u64,
    },
    
    Connections {
        #[arg(short, long, default_value = "ws://127.0.0.1:8080")]
        url: String,
        
        #[arg(short = 'x', long, default_value = "100")]
        max_connections: usize,
        
        #[arg(short, long, default_value = "10")]
        step: usize,
        
        #[arg(short, long, default_value = "5")]
        hold_duration: u64,
    },
    
    Frequency {
        #[arg(short = 'u', long, default_value = "ws://127.0.0.1:8080")]
        url: String,
        
        #[arg(short = 'f', long, default_value = "20")]
        frequency: u64,
        
        #[arg(short = 'd', long, default_value = "30")]
        duration_seconds: u64,
        
        #[arg(short = 'p', long, default_value = "100")]
        payload_size: usize,
        
        #[arg(short = 'c', long, default_value = "10")]
        connections: usize,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BenchmarkMessage {
    id: u64,
    timestamp: u64,
    payload: String,
}

#[derive(Debug, Default)]
struct BenchmarkStats {
    messages_sent: u64,
    messages_received: u64,
    total_latency_ms: u64,
    min_latency_ms: u64,
    max_latency_ms: u64,
    start_time: Option<Instant>,
    end_time: Option<Instant>,
}

// Sharded stats to reduce lock contention
#[derive(Debug)]
struct ShardedBenchmarkStats {
    // Atomic counters for high-frequency operations
    messages_sent: AtomicU64,
    messages_received: AtomicU64,
    total_latency_ms: AtomicU64,
    
    // Sharded mutexes for less frequent updates (4 shards should be enough)
    shards: [Mutex<BenchmarkStatsShard>; 4],
    
    // Global state (protected by mutex)
    global: Mutex<BenchmarkStatsGlobal>,
}

#[derive(Debug, Default)]
struct BenchmarkStatsShard {
    min_latency_ms: u64,
    max_latency_ms: u64,
}

#[derive(Debug, Default)]
struct BenchmarkStatsGlobal {
    start_time: Option<Instant>,
    end_time: Option<Instant>,
}

impl ShardedBenchmarkStats {
    fn new() -> Self {
        Self {
            messages_sent: AtomicU64::new(0),
            messages_received: AtomicU64::new(0),
            total_latency_ms: AtomicU64::new(0),
            shards: [
                Mutex::new(BenchmarkStatsShard::default()),
                Mutex::new(BenchmarkStatsShard::default()),
                Mutex::new(BenchmarkStatsShard::default()),
                Mutex::new(BenchmarkStatsShard::default()),
            ],
            global: Mutex::new(BenchmarkStatsGlobal::default()),
        }
    }
    
    fn increment_sent(&self) {
        self.messages_sent.fetch_add(1, Ordering::Relaxed);
    }
    
    fn increment_received(&self) {
        self.messages_received.fetch_add(1, Ordering::Relaxed);
    }
    
    async fn add_latency(&self, latency_ms: u64, shard_id: usize) {
        self.total_latency_ms.fetch_add(latency_ms, Ordering::Relaxed);
        
        // Use sharded mutex for min/max updates (less frequent)
        let shard_idx = shard_id % 4;
        let mut shard = self.shards[shard_idx].lock().await;
        if shard.min_latency_ms == 0 || latency_ms < shard.min_latency_ms {
            shard.min_latency_ms = latency_ms;
        }
        if latency_ms > shard.max_latency_ms {
            shard.max_latency_ms = latency_ms;
        }
    }
    
    async fn set_start_time(&self, time: Instant) {
        let mut global = self.global.lock().await;
        global.start_time = Some(time);
    }
    
    async fn set_end_time(&self, time: Instant) {
        let mut global = self.global.lock().await;
        global.end_time = Some(time);
    }
    
    async fn to_benchmark_stats(&self) -> BenchmarkStats {
        let messages_sent = self.messages_sent.load(Ordering::Relaxed);
        let messages_received = self.messages_received.load(Ordering::Relaxed);
        let total_latency_ms = self.total_latency_ms.load(Ordering::Relaxed);
        
        // Aggregate min/max from all shards
        let mut min_latency_ms = u64::MAX;
        let mut max_latency_ms = 0u64;
        
        for shard_mutex in &self.shards {
            let shard = shard_mutex.lock().await;
            if shard.min_latency_ms > 0 && shard.min_latency_ms < min_latency_ms {
                min_latency_ms = shard.min_latency_ms;
            }
            if shard.max_latency_ms > max_latency_ms {
                max_latency_ms = shard.max_latency_ms;
            }
        }
        
        if min_latency_ms == u64::MAX {
            min_latency_ms = 0;
        }
        
        let global = self.global.lock().await;
        BenchmarkStats {
            messages_sent,
            messages_received,
            total_latency_ms,
            min_latency_ms,
            max_latency_ms,
            start_time: global.start_time,
            end_time: global.end_time,
        }
    }
}

impl BenchmarkStats {
    fn add_latency(&mut self, latency_ms: u64) {
        self.total_latency_ms += latency_ms;
        if self.min_latency_ms == 0 || latency_ms < self.min_latency_ms {
            self.min_latency_ms = latency_ms;
        }
        if latency_ms > self.max_latency_ms {
            self.max_latency_ms = latency_ms;
        }
    }
    
    fn average_latency_ms(&self) -> f64 {
        if self.messages_received == 0 {
            0.0
        } else {
            self.total_latency_ms as f64 / self.messages_received as f64
        }
    }
    
    fn throughput_mps(&self) -> f64 {
        if let (Some(start), Some(end)) = (self.start_time, self.end_time) {
            let duration = end.duration_since(start).as_secs_f64();
            if duration > 0.0 {
                self.messages_received as f64 / duration
            } else {
                0.0
            }
        } else {
            0.0
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    
    let args = Args::parse();
    
    match args.command {
        Commands::Throughput { url, messages, payload_size, connections, delay_ms } => {
            run_throughput_benchmark(url, messages, payload_size, connections, delay_ms).await;
        }
        Commands::Connections { url, max_connections, step, hold_duration } => {
            run_connection_benchmark(url, max_connections, step, hold_duration).await;
        }
        Commands::Frequency { url, frequency, duration_seconds, payload_size, connections } => {
            run_frequency_benchmark(url, frequency, duration_seconds, payload_size, connections).await;
        }
    }
}

async fn run_throughput_benchmark(
    url: String,
    messages_per_connection: u64,
    payload_size: usize,
    num_connections: usize,
    delay_ms: u64,
) {
    info!("Starting throughput benchmark:");
    info!("  URL: {}", url);
    info!("  Messages per connection: {}", messages_per_connection);
    info!("  Payload size: {} bytes", payload_size);
    info!("  Connections: {}", num_connections);
    info!("  Delay between messages: {}ms", delay_ms);
    
    let payload = "x".repeat(payload_size);
    let stats = Arc::new(Mutex::new(BenchmarkStats::default()));
    let mut handles = Vec::new();
    
    let start_time = Instant::now();
    stats.lock().await.start_time = Some(start_time);
    
    for conn_id in 0..num_connections {
        let url = url.clone();
        let payload = payload.clone();
        let stats = stats.clone();
        
        let handle = tokio::spawn(async move {
            if let Err(e) = run_throughput_connection(
                url, 
                conn_id, 
                messages_per_connection, 
                payload, 
                delay_ms,
                stats
            ).await {
                error!("Connection {} failed: {}", conn_id, e);
            }
        });
        
        handles.push(handle);
    }
    
    for handle in handles {
        let _ = handle.await;
    }
    
    let end_time = Instant::now();
    let mut stats_lock = stats.lock().await;
    stats_lock.end_time = Some(end_time);
    
    let duration = end_time.duration_since(start_time);
    
    info!("\n=== Throughput Benchmark Results ===");
    info!("Total duration: {:.2}s", duration.as_secs_f64());
    info!("Messages sent: {}", stats_lock.messages_sent);
    info!("Messages received: {}", stats_lock.messages_received);
    info!("Message loss: {:.2}%", 
        (stats_lock.messages_sent - stats_lock.messages_received) as f64 / stats_lock.messages_sent as f64 * 100.0);
    info!("Throughput: {:.2} messages/second", stats_lock.throughput_mps());
    info!("Average latency: {:.2}ms", stats_lock.average_latency_ms());
    info!("Min latency: {}ms", stats_lock.min_latency_ms);
    info!("Max latency: {}ms", stats_lock.max_latency_ms);
}

async fn run_throughput_connection(
    url: String,
    conn_id: usize,
    num_messages: u64,
    payload: String,
    delay_ms: u64,
    stats: Arc<Mutex<BenchmarkStats>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (ws_stream, _) = connect_async(&url).await?;
    let (mut sender, mut receiver) = ws_stream.split();
    
    let stats_clone = stats.clone();
    let receive_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(benchmark_msg) = serde_json::from_str::<BenchmarkMessage>(&text) {
                        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
                        let latency = now.saturating_sub(benchmark_msg.timestamp);
                        
                        let mut stats_lock = stats_clone.lock().await;
                        stats_lock.messages_received += 1;
                        stats_lock.add_latency(latency);
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    warn!("Connection {} receive error: {}", conn_id, e);
                    break;
                }
                _ => {}
            }
        }
    });
    
    for i in 0..num_messages {
        let message = BenchmarkMessage {
            id: i,
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
            payload: payload.clone(),
        };
        
        let message_text = serde_json::to_string(&message)?;
        sender.send(Message::Text(message_text)).await?;
        
        {
            let mut stats_lock = stats.lock().await;
            stats_lock.messages_sent += 1;
        }
        
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = sender.close().await;
    let _ = receive_task.await;
    
    Ok(())
}

async fn run_connection_benchmark(
    url: String,
    max_connections: usize,
    step: usize,
    hold_duration: u64,
) {
    info!("Starting connection benchmark:");
    info!("  URL: {}", url);
    info!("  Max connections: {}", max_connections);
    info!("  Step size: {}", step);
    info!("  Hold duration: {}s", hold_duration);
    
    let mut current_connections = step;
    
    while current_connections <= max_connections {
        info!("\nTesting {} concurrent connections...", current_connections);
        
        let start_time = Instant::now();
        let mut handles = Vec::new();
        
        for conn_id in 0..current_connections {
            let url = url.clone();
            
            let handle = tokio::spawn(async move {
                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        let (mut sender, mut receiver) = ws_stream.split();
                        
                        let ping_task = tokio::spawn(async move {
                            let mut interval = tokio::time::interval(Duration::from_secs(1));
                            loop {
                                interval.tick().await;
                                if sender.send(Message::Ping(vec![])).await.is_err() {
                                    break;
                                }
                            }
                        });
                        
                        let receive_task = tokio::spawn(async move {
                            while let Some(msg) = receiver.next().await {
                                match msg {
                                    Ok(Message::Close(_)) => break,
                                    Err(_) => break,
                                    _ => {}
                                }
                            }
                        });
                        
                        tokio::time::sleep(Duration::from_secs(hold_duration)).await;
                        
                        ping_task.abort();
                        receive_task.abort();
                        
                        true
                    }
                    Err(e) => {
                        error!("Connection {} failed: {}", conn_id, e);
                        false
                    }
                }
            });
            
            handles.push(handle);
        }
        
        let mut successful_connections = 0;
        for handle in handles {
            if let Ok(success) = handle.await {
                if success {
                    successful_connections += 1;
                }
            }
        }
        
        let duration = start_time.elapsed();
        
        info!("Results for {} connections:", current_connections);
        info!("  Successful: {}/{}", successful_connections, current_connections);
        info!("  Success rate: {:.2}%", successful_connections as f64 / current_connections as f64 * 100.0);
        info!("  Connection time: {:.2}s", duration.as_secs_f64());
        
        current_connections += step;
    }
    
    info!("\n=== Connection Benchmark Complete ===");
}

async fn run_frequency_benchmark(
    url: String,
    frequency: u64,
    duration_seconds: u64,
    payload_size: usize,
    num_connections: usize,
) {
    info!("Starting frequency benchmark:");
    info!("  URL: {}", url);
    info!("  Frequency: {}Hz ({}ms intervals)", frequency, 1000 / frequency);
    info!("  Duration: {}s", duration_seconds);
    info!("  Payload size: {} bytes", payload_size);
    info!("  Connections: {}", num_connections);
    
    let payload = "x".repeat(payload_size);
    let stats = Arc::new(ShardedBenchmarkStats::new());
    let mut handles = Vec::new();
    
    let start_time = Instant::now();
    stats.set_start_time(start_time).await;
    
    let total_messages = frequency * duration_seconds;
    
    for conn_id in 0..num_connections {
        let url = url.clone();
        let payload = payload.clone();
        let stats = stats.clone();
        
        let handle = tokio::spawn(async move {
            if let Err(e) = run_frequency_connection(
                url, 
                conn_id, 
                frequency,
                duration_seconds,
                payload, 
                stats
            ).await {
                error!("Connection {} failed: {}", conn_id, e);
            }
        });
        
        handles.push(handle);
    }
    
    // Monitor progress periodically
    let stats_clone = stats.clone();
    let monitor_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let start = Instant::now();
        
        loop {
            interval.tick().await;
            let elapsed = start.elapsed().as_secs();
            
            if elapsed >= duration_seconds {
                break;
            }
            
            let current_stats = stats_clone.to_benchmark_stats().await;
            let expected_echo = num_connections as u64 * frequency * elapsed;
            let expected_broadcasts = 20u64 * elapsed; // Server broadcasts at 20Hz
            let _expected_total = expected_echo + expected_broadcasts;
            let actual = current_stats.messages_received;
            let loss_rate = if expected_echo > 0 {
                if actual >= expected_echo {
                    0.0 // Received at least all expected echo messages
                } else {
                    (expected_echo - actual) as f64 / expected_echo as f64 * 100.0
                }
            } else {
                0.0
            };
            
            info!(
                "Progress: {}s/{} - Sent: {}, Received: {}, Loss: {:.2}%, Avg Latency: {:.2}ms",
                elapsed, duration_seconds, current_stats.messages_sent, actual, 
                loss_rate, current_stats.average_latency_ms()
            );
        }
    });
    
    for handle in handles {
        let _ = handle.await;
    }
    
    monitor_handle.abort();
    
    let end_time = Instant::now();
    stats.set_end_time(end_time).await;
    
    let final_stats = stats.to_benchmark_stats().await;
    let duration = end_time.duration_since(start_time);
    let expected_echo = num_connections as u64 * total_messages;
    let expected_broadcasts = 20u64 * duration.as_secs(); // Server broadcasts at 20Hz
    let expected_total = expected_echo + expected_broadcasts;
    let loss_rate = if final_stats.messages_received >= expected_echo {
        0.0 // Received at least all expected echo messages
    } else {
        (expected_echo - final_stats.messages_received) as f64 / expected_echo as f64 * 100.0
    };
    
    info!("\n=== {}Hz Frequency Benchmark Results ===", frequency);
    info!("Total duration: {:.2}s", duration.as_secs_f64());
    info!("Expected echo messages: {}", expected_echo);
    info!("Expected broadcast messages: {}", expected_broadcasts);
    info!("Expected total messages: {}", expected_total);
    info!("Messages sent: {}", final_stats.messages_sent);
    info!("Messages received: {}", final_stats.messages_received);
    info!("Echo message loss: {:.2}%", loss_rate);
    info!("Actual frequency: {:.2}Hz", final_stats.throughput_mps());
    info!("Average latency: {:.2}ms", final_stats.average_latency_ms());
    info!("Min latency: {}ms", final_stats.min_latency_ms);
    info!("Max latency: {}ms", final_stats.max_latency_ms);
}

async fn run_frequency_connection(
    url: String,
    conn_id: usize,
    frequency: u64,
    duration_seconds: u64,
    payload: String,
    stats: Arc<ShardedBenchmarkStats>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Add connection timeout (keep original simple approach)
    let (ws_stream, _) = tokio::time::timeout(
        Duration::from_secs(30), 
        connect_async(&url)
    ).await.map_err(|_| "Connection timeout")??;
    
    let (mut sender, mut receiver) = ws_stream.split();
    
    let stats_clone = stats.clone();
    let receive_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    if let Ok(benchmark_msg) = serde_json::from_str::<BenchmarkMessage>(&text) {
                        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
                        let latency = now.saturating_sub(benchmark_msg.timestamp);
                        
                        stats_clone.increment_received();
                        stats_clone.add_latency(latency, conn_id).await;
                    }
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    warn!("Connection {} receive error: {}", conn_id, e);
                    break;
                }
                _ => {}
            }
        }
    });
    
    let interval_ms = 1000 / frequency;
    let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
    let end_time = Instant::now() + Duration::from_secs(duration_seconds);
    let mut message_id = 0u64;
    
    info!("Connection {} started - sending at {}Hz", conn_id, frequency);
    
    while Instant::now() < end_time {
        interval.tick().await;
        
        let message = BenchmarkMessage {
            id: message_id,
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
            payload: payload.clone(),
        };
        
        let message_text = serde_json::to_string(&message)?;
        
        match sender.send(Message::Text(message_text)).await {
            Ok(_) => {
                stats.increment_sent();
                message_id += 1;
            }
            Err(e) => {
                error!("Connection {} send error: {}", conn_id, e);
                break;
            }
        }
    }
    
    info!("Connection {} finished - sent {} messages", conn_id, message_id);
    
    // Wait a bit for remaining responses
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = sender.close().await;
    let _ = receive_task.await;
    
    Ok(())
}