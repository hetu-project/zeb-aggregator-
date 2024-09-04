use std::cmp::Ordering;
use crate::behaviour::{NodeBehaviour, NodeEvent};
use libp2p::{gossipsub, identity, swarm::{SwarmEvent, Swarm}, Multiaddr, PeerId, Transport};
use libp2p::{tcp, noise, yamux};
use libp2p::core::upgrade;
use serde::{Serialize, Deserialize};
use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use tokio::sync::mpsc;
use tracing::{info, warn, error};
use config::Config;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::interval;
use base64::{Engine as _, engine::general_purpose};
use jsonrpc_core::futures_util::StreamExt;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DataWithClock {
    pub data: String,
    #[serde(with = "peer_id_serde")]
    pub vector_clock: HashMap<PeerId, u64>,
    pub timestamp: u64,
}

impl PartialOrd for DataWithClock {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DataWithClock {
    fn cmp(&self, other: &Self) -> Ordering {
        // First, compare using vector clocks
        let mut self_greater = false;
        let mut other_greater = false;

        for (peer, &self_count) in &self.vector_clock {
            match other.vector_clock.get(peer) {
                Some(&other_count) => {
                    if self_count > other_count {
                        self_greater = true;
                    } else if other_count > self_count {
                        other_greater = true;
                    }
                }
                None => self_greater = true,
            }
        }

        for peer in other.vector_clock.keys() {
            if !self.vector_clock.contains_key(peer) {
                other_greater = true;
            }
        }

        match (self_greater, other_greater) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => Ordering::Equal,
            // If vector clocks are concurrent, use timestamp as tie-breaker
            (true, true) => self.timestamp.cmp(&other.timestamp),
        }
    }
}

impl PartialEq for DataWithClock {
    fn eq(&self, other: &Self) -> bool {
        self.vector_clock == other.vector_clock
    }
}

impl Eq for DataWithClock {}
mod peer_id_serde {
    use libp2p::PeerId;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::HashMap;
    use std::str::FromStr;

    pub fn serialize<S>(
        vector_clock: &HashMap<PeerId, u64>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let stringified: HashMap<String, u64> = vector_clock
            .iter()
            .map(|(k, v)| (k.to_base58(), *v))
            .collect();
        stringified.serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<HashMap<PeerId, u64>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let stringified: HashMap<String, u64> = HashMap::deserialize(deserializer)?;
        stringified
            .into_iter()
            .map(|(k, v)| {
                Ok((
                    PeerId::from_str(&k).map_err(serde::de::Error::custom)?,
                    v,
                ))
            })
            .collect()
    }
}

pub struct Node {
    swarm: Swarm<NodeBehaviour>,
    vector_clock: HashMap<PeerId, u64>,
    topic: gossipsub::IdentTopic,
    stored_data: BTreeMap<DataWithClock, ()>, // Use DataWithClock as the key for sorting
}

impl Node {
    pub fn new(swarm: Swarm<NodeBehaviour>) -> Self {
        Node {
            swarm,
            vector_clock: HashMap::new(),
            topic: gossipsub::IdentTopic::new("relay_data"),
            stored_data: BTreeMap::new(),
        }
    }

    pub async fn create(config: &Config) -> Result<(Self, PeerId), Box<dyn Error + Send + Sync>> {
        let private_key_str = config.get_string("node.private_key")?;
        let id_keys = if private_key_str.is_empty() {
            let keys = identity::Keypair::generate_ed25519();
            let encoded_private_key = general_purpose::STANDARD.encode(keys.to_protobuf_encoding()?);
            info!("Generated new private key. Add this to your config to reuse the same peer ID:");
            info!("private_key = \"{}\"", encoded_private_key);
            keys
        } else {
            let decoded_key = general_purpose::STANDARD.decode(private_key_str)?;
            identity::Keypair::from_protobuf_encoding(&decoded_key)?
        };

        let peer_id = PeerId::from(id_keys.public());
        info!(?peer_id, "Using peer ID");

        let transport = tcp::tokio::Transport::default()
            .upgrade(upgrade::Version::V1)
            .authenticate(noise::Config::new(&id_keys).expect("signing libp2p-noise static keypair"))
            .multiplex(yamux::Config::default())
            .boxed();

        let behaviour = NodeBehaviour::new(&id_keys, peer_id);

        let swarm_config = libp2p::swarm::Config::with_tokio_executor()
            .with_idle_connection_timeout(Duration::from_secs(3000));
        let mut swarm = Swarm::new(transport, behaviour, peer_id, swarm_config);
        let p2p_port = config.get_int("network.p2p_port")? as u16;
        swarm.listen_on(format!("/ip4/0.0.0.0/tcp/{}", p2p_port).parse()?)?;

        let mut node = Node::new(swarm);
        node.subscribe_to_topic();

        Ok((node, peer_id))
    }

    pub async fn start(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        loop {
            match self.swarm.select_next_some().await {
                SwarmEvent::Behaviour(NodeEvent::PeerDiscovered(peer_id, addrs)) => {
                    info!("Discovered peer: {:?} at {:?}", peer_id, addrs);
                    for addr in addrs {
                        match self.swarm.dial(addr.clone()) {
                            Ok(_) => info!("Dialing discovered peer: {:?} at {:?}", peer_id, addr),
                            Err(e) => warn!("Failed to dial discovered peer: {:?} at {:?}. Error: {:?}", peer_id, addr, e),
                        }
                    }
                }
                SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                    info!("Connected to peer: {:?} via {:?}", peer_id, endpoint);
                }
                SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                    info!("Disconnected from peer: {:?}. Cause: {:?}", peer_id, cause);
                }
                SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                    warn!("Failed to connect to peer {:?}. Error: {:?}", peer_id, error);
                }
                SwarmEvent::Behaviour(NodeEvent::Gossipsub(gossipsub::Event::Message {
                                                               propagation_source,
                                                               message_id,
                                                               message
                                                           })) => {
                    if let Ok(data) = serde_json::from_slice::<DataWithClock>(&message.data) {
                        self.process_received_data(data).await?;
                    }
                }
                _ => {}
            }
        }
    }

    pub async fn process_received_data(&mut self, mut data: DataWithClock) -> Result<(), Box<dyn Error + Send + Sync>> {
        info!("Received data from peer: {:?}", data);

        let local_peer_id = *self.swarm.local_peer_id();

        // Check if the received data is newer based on vector clock
        let mut is_new_data = true;
        for (&peer_id, &received_clock) in &data.vector_clock {
            if let Some(&local_clock) = self.vector_clock.get(&peer_id) {
                if received_clock <= local_clock {
                    is_new_data = false;
                    break;
                }
            }
        }

        // If it's new data, update our clock and republish
        if is_new_data {
            // Update our vector clock
            for (peer_id, &received_clock) in &data.vector_clock {
                let local_clock = self.vector_clock.entry(*peer_id).or_insert(0);
                *local_clock = std::cmp::max(*local_clock, received_clock);
            }
            let local_clock = self.vector_clock.entry(local_peer_id).or_insert(0);
            *local_clock += 1;

            // Log the updated vector clock
            info!("Updated vector clock: {:?}", self.vector_clock);

            // Update the received data's vector clock and timestamp
            data.vector_clock = self.vector_clock.clone();
            data.timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_secs();

            // Store the data
            self.stored_data.insert(data.clone(), ());

            // Log that we're republishing the data
            info!("Republishing new data: {:?}", data);

            self.publish_data(data).await?;
        } else {
            info!("Received data is not newer than current state. Not republishing.");
        }

        Ok(())
    }

    pub fn get_sorted_data(&self) -> Vec<&DataWithClock> {
        self.stored_data.keys().collect()
    }

    pub async fn publish_data(&mut self, data: DataWithClock) -> Result<(), Box<dyn Error + Send + Sync>> {
        let message = serde_json::to_string(&data)?;
        self.swarm.behaviour_mut().gossipsub.publish(self.topic.clone(), message.as_bytes())?;
        Ok(())
    }

    pub fn subscribe_to_topic(&mut self) {
        if let Err(e) = self.swarm.behaviour_mut().gossipsub.subscribe(&self.topic) {
            error!("Failed to subscribe to topic: {:?}", e);
        } else {
            info!("Subscribed to topic: {:?}", self.topic);
        }
    }

    pub async fn connect_to_bootstrap_peers(&mut self, config: &Config) -> Result<(), Box<dyn Error + Send + Sync>> {
        if let Ok(bootstrap_peers) = config.get_array("node.bootstrap_peers") {
            for peer in bootstrap_peers {
                if let Ok(addr) = peer.into_string() {
                    match addr.parse::<Multiaddr>() {
                        Ok(multiaddr) => {
                            info!("Connecting to bootstrap peer: {}", addr);
                            match self.swarm.dial(multiaddr.clone()) {
                                Ok(_) => info!("Dialing peer: {}", addr),
                                Err(e) => warn!("Failed to dial peer {}: {:?}", addr, e),
                            }
                        }
                        Err(e) => {
                            warn!("Failed to parse bootstrap peer address {}: {:?}", addr, e);
                        }
                    }
                }
            }
        } else {
            info!("No bootstrap peers configured");
        }
        Ok(())
    }

    pub async fn print_node_addresses(&self, config: &Config) -> Result<(), Box<dyn Error + Send + Sync>> {
        let local_peer_id = *self.swarm.local_peer_id();
        let listened_addrs = self.swarm.listeners().cloned().collect::<Vec<_>>();
        let p2p_port = config.get_int("network.p2p_port")? as u16;
        let external_ip = config.get_string("network.external_ip")?;

        if listened_addrs.is_empty() {
            warn!("No external addresses found. The node might not be publicly accessible.");
        } else {
            info!("Node addresses:");
            for addr in listened_addrs {
                info!("{}/p2p/{}", addr, local_peer_id);
            }
        }

        info!("External IP address (from config): {}", external_ip);
        info!("Full address with external IP: /ip4/{}/tcp/{}/p2p/{}", external_ip, p2p_port, local_peer_id);

        info!("You can share these addresses with others to allow them to connect to your node.");
        Ok(())
    }

    pub async fn run(&mut self, rx: &mut mpsc::Receiver<String>) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut interval = interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let connected_peers: Vec<_> = self.swarm.connected_peers().collect();
                    info!("Connected peers: {} - {:?}", connected_peers.len(), connected_peers);

                    // Log GossipSub information
                    let gossipsub = &self.swarm.behaviour().gossipsub;
                    let topic_peers = gossipsub.topics().fold(0, |acc, topic| acc + gossipsub.mesh_peers(topic).count());
                    let all_peers = gossipsub.all_peers().count();
                    info!("GossipSub info:");
                    info!("  Topics: {:?}", gossipsub.topics().collect::<Vec<_>>());
                    info!("  Peers in topics: {}", topic_peers);
                    info!("  All known peers: {}", all_peers);
                    info!("  Vector clock: {:?}", self.vector_clock);

                    // Log sorted data
                    let sorted_data = self.get_sorted_data();
                    info!("Stored data (sorted by vector clock):");
                    for data in sorted_data {
                        info!("  Vector Clock: {:?}, Timestamp: {}, Data: {}", data.vector_clock, data.timestamp, data.data);
                    }
                }

                result = self.start() => {
                    if let Err(e) = result {
                        error!("Node error: {:?}", e);
                        return Err(e.into());
                    }
                }
                Some(message) = rx.recv() => {
                    // Log the received data and vector clock
                    info!("Received data from RPC: {}", message);
                    info!("Current vector clock: {:?}", self.vector_clock);

                    let local_peer_id = *self.swarm.local_peer_id();
                    let local_clock = self.vector_clock.entry(local_peer_id).or_insert(0);
                    *local_clock += 1;

                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("Time went backwards")
                        .as_secs();

                    let data_with_clock = DataWithClock {
                        data: message,
                        vector_clock: self.vector_clock.clone(),
                        timestamp,
                    };

                    // Log the data being published
                    info!("Publishing data: {:?}", data_with_clock);

                    if let Err(e) = self.publish_data(data_with_clock).await {
                        error!("Failed to publish data: {:?}", e);
                    }
                }
            }
        }
    }
}