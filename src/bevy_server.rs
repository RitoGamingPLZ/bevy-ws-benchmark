use bevy::prelude::*;
use clap::Parser;

mod ecs;
use ecs::*;

pub struct WebSocketPlugin {
    pub args: systems::Args,
}

impl Plugin for WebSocketPlugin {
    fn build(&self, app: &mut App) {
        app.add_event::<MessageReceived>()
            .add_event::<ConnectionClosed>()
            .add_event::<BroadcastMessage>()
            .insert_resource(WebSocketServer::new())
            .insert_resource(BroadcastConfig { enabled: self.args.broadcast })
            .insert_resource(ServerConfig {
                bind: self.args.bind.clone(),
                stats: self.args.stats,
                broadcast: self.args.broadcast,
                broadcast_size: self.args.broadcast_size,
            })
            .insert_resource(SharedRuntime::new())
            // Set fixed timestep to 20Hz (50ms)
            .insert_resource(Time::<Fixed>::from_hz(20.0))
            .add_systems(Startup, systems::setup_websocket_server)
            .add_systems(
                Update,
                (
                    systems::handle_new_connections,
                    systems::handle_disconnections,
                ),
            )
            .add_systems(
                FixedUpdate,
                (
                    systems::broadcast_system,
                    systems::handle_broadcast_messages
                        .after(systems::broadcast_system),
                    systems::update_stats
                        .after(systems::broadcast_system)
                )
            );
    }
}


fn main() {
    // Initialize tracing with explicit configuration
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "rust_ws_benchmark=info,bevy=warn".to_string())
        )
        .init();
    
    // Parse command line arguments
    let args = systems::Args::parse();
    
    info!("Starting Bevy WebSocket server on {}", args.bind);
    info!("Stats enabled: {}", args.stats);
    info!("Broadcasting enabled: {}", args.broadcast);
    if args.broadcast {
        info!("Broadcast message size: {} bytes", args.broadcast_size);
    }
    
    App::new()
        .add_plugins((
            MinimalPlugins,
            WebSocketPlugin { args },
        ))
        .run();
}