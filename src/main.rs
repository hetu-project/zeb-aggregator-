use crate::node::Node;
use crate::config::load_config;
use crate::rpc::run_json_rpc_server;
use std::error::Error;
use tokio::sync::mpsc;
use tracing::{info, error};
use std::{env, process};

mod node;
mod behaviour;
mod config;
mod rpc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = env::args().collect();
    let config_path = args.get(1).cloned().unwrap_or_else(|| "config.toml".to_string());

    let config = load_config(&config_path)?;
    let (mut node, peer_id) = Node::create(&config).await?;

    let (tx, mut rx) = mpsc::channel::<String>(100);

    let rpc_port = config.get_int("network.rpc_port")? as u16;
    let rpc_address = format!("127.0.0.1:{}", rpc_port).parse()?;
    tokio::spawn(run_json_rpc_server(rpc_address, tx.clone()));

    let p2p_port = config.get_int("network.p2p_port")? as u16;
    info!("P2P node {} listening on /ip4/0.0.0.0/tcp/{}", peer_id, p2p_port);

    // Connect to bootstrap peers
    node.connect_to_bootstrap_peers(&config).await?;

    // Wait a bit for the node to establish connections
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Print node addresses
    node.print_node_addresses(&config).await?;

    let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel::<()>();
    let shutdown_signal = async move {
        tokio::signal::ctrl_c().await.expect("Failed to install Ctrl+C handler");
        shutdown_sender.send(()).expect("Failed to send shutdown signal");
    };

    tokio::select! {
        _ = shutdown_signal => {
            info!("Received Ctrl+C, shutting down");
        }
        _ = shutdown_receiver => {
            info!("Shutdown signal received");
        }
        result = node.run(&mut rx) => {
            if let Err(e) = result {
                error!("Node error: {:?}", e);
            }
        }
    }

    process::exit(0);
}