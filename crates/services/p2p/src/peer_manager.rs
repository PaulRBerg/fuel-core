use fuel_core_types::{
    fuel_types::BlockHeight,
    services::p2p::peer_reputation::{
        AppScore,
        DECAY_APP_SCORE,
        DEFAULT_APP_SCORE,
        MAX_APP_SCORE,
        MIN_APP_SCORE,
    },
};
use libp2p::{
    Multiaddr,
    PeerId,
};
use rand::seq::IteratorRandom;
use std::{
    collections::{
        HashMap,
        HashSet,
    },
    sync::{
        Arc,
        RwLock,
    },
    time::Duration,
};
use tokio::time::Instant;
use tracing::{
    debug,
    info,
};

use crate::gossipsub_config::GRAYLIST_THRESHOLD;

/// At this point we better just ban the peer
const MIN_GOSSIPSUB_SCORE_BEFORE_BAN: AppScore = GRAYLIST_THRESHOLD;

// Info about a single Peer that we're connected to
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub peer_addresses: HashSet<Multiaddr>,
    pub client_version: Option<String>,
    pub heartbeat_data: HeartbeatData,
    pub score: AppScore,
}

impl Default for PeerInfo {
    fn default() -> Self {
        Self {
            score: DEFAULT_APP_SCORE,
            client_version: Default::default(),
            heartbeat_data: Default::default(),
            peer_addresses: Default::default(),
        }
    }
}

enum PeerInfoInsert {
    Addresses(Vec<Multiaddr>),
    ClientVersion(String),
    HeartbeatData(HeartbeatData),
}

/// Manages Peers and their events
#[derive(Debug)]
pub struct PeerManager {
    score_config: ScoreConfig,
    non_reserved_connected_peers: HashMap<PeerId, PeerInfo>,
    reserved_connected_peers: HashMap<PeerId, PeerInfo>,
    reserved_peers: HashSet<PeerId>,
    connection_state: Arc<RwLock<ConnectionState>>,
    max_non_reserved_peers: usize,
    reserved_peers_updates: tokio::sync::broadcast::Sender<usize>,
}

impl PeerManager {
    pub fn new(
        reserved_peers: HashSet<PeerId>,
        connection_state: Arc<RwLock<ConnectionState>>,
        max_non_reserved_peers: usize,
    ) -> Self {
        let (reserved_peers_updates, _) =
            tokio::sync::broadcast::channel(1 + reserved_peers.len() * 2);

        Self {
            score_config: ScoreConfig::default(),
            non_reserved_connected_peers: HashMap::with_capacity(max_non_reserved_peers),
            reserved_connected_peers: HashMap::with_capacity(reserved_peers.len()),
            reserved_peers,
            connection_state,
            max_non_reserved_peers,
            reserved_peers_updates,
        }
    }

    pub fn reserved_peers_updates(&self) -> tokio::sync::broadcast::Sender<usize> {
        self.reserved_peers_updates.clone()
    }

    pub fn handle_gossip_score_update<T: Punisher>(
        &self,
        peer_id: PeerId,
        gossip_score: f64,
        punisher: &mut T,
    ) {
        if gossip_score < self.score_config.min_gossip_score_allowed
            && !self.reserved_peers.contains(&peer_id)
        {
            punisher.ban_peer(peer_id);
        }
    }

    pub fn handle_peer_info_updated(
        &mut self,
        peer_id: &PeerId,
        block_height: BlockHeight,
    ) {
        if let Some(time_elapsed) = self
            .get_peer_info(peer_id)
            .and_then(|info| info.heartbeat_data.seconds_since_last_heartbeat())
        {
            debug!(target: "fuel-p2p", "Previous heartbeat happened {:?} seconds ago", time_elapsed);
        }

        let heartbeat_data = HeartbeatData::new(block_height);

        self.insert_peer_info(peer_id, PeerInfoInsert::HeartbeatData(heartbeat_data));
    }

    /// Returns `true` signaling that the peer should be disconnected
    pub fn handle_peer_connected(
        &mut self,
        peer_id: &PeerId,
        addresses: Vec<Multiaddr>,
        initial_connection: bool,
    ) -> bool {
        if initial_connection {
            self.handle_initial_connection(peer_id, addresses)
        } else {
            self.insert_peer_info(peer_id, PeerInfoInsert::Addresses(addresses));
            false
        }
    }

    pub fn handle_peer_identified(
        &mut self,
        peer_id: &PeerId,
        addresses: Vec<Multiaddr>,
        agent_version: String,
    ) {
        self.insert_peer_info(peer_id, PeerInfoInsert::ClientVersion(agent_version));
        self.insert_peer_info(peer_id, PeerInfoInsert::Addresses(addresses));
    }

    pub fn batch_update_score_with_decay(&mut self) {
        for peer_info in self.non_reserved_connected_peers.values_mut() {
            peer_info.score *= DECAY_APP_SCORE;
        }
    }

    pub fn update_app_score<T: Punisher>(
        &mut self,
        peer_id: PeerId,
        score: AppScore,
        reporting_service: &str,
        punisher: &mut T,
    ) {
        if let Some(peer) = self.non_reserved_connected_peers.get_mut(&peer_id) {
            // score should not go over `max_score`
            let new_score = self.score_config.max_app_score.min(peer.score + score);
            peer.score = new_score;

            info!(target: "fuel-p2p", "{reporting_service} updated {peer_id} with new score {score}");

            if new_score < self.score_config.min_app_score_allowed {
                punisher.ban_peer(peer_id);
            }
        } else {
            log_missing_peer(&peer_id);
        }
    }

    pub fn total_peers_connected(&self) -> usize {
        self.reserved_connected_peers.len() + self.non_reserved_connected_peers.len()
    }

    pub fn get_peers_ids(&self) -> impl Iterator<Item = &PeerId> {
        self.non_reserved_connected_peers
            .keys()
            .chain(self.reserved_connected_peers.keys())
    }

    pub fn get_peer_info(&self, peer_id: &PeerId) -> Option<&PeerInfo> {
        if self.reserved_peers.contains(peer_id) {
            return self.reserved_connected_peers.get(peer_id)
        }
        self.non_reserved_connected_peers.get(peer_id)
    }

    pub fn get_disconnected_reserved_peers(&self) -> impl Iterator<Item = &PeerId> {
        self.reserved_peers
            .iter()
            .filter(|peer_id| !self.reserved_connected_peers.contains_key(peer_id))
    }

    /// Handles on peer's last connection getting disconnected
    /// Returns 'true' signaling we should try reconnecting
    pub fn handle_peer_disconnect(&mut self, peer_id: PeerId) -> bool {
        // try immediate reconnect if it's a reserved peer
        let is_reserved = self.reserved_peers.contains(&peer_id);

        if !is_reserved {
            // check were all the slots taken prior to this disconnect
            let all_slots_taken = self.max_non_reserved_peers
                == self.non_reserved_connected_peers.len() + 1;

            if self.non_reserved_connected_peers.remove(&peer_id).is_some()
                && all_slots_taken
            {
                // since all the slots were full prior to this disconnect
                // let's allow new peer non-reserved peers connections
                if let Ok(mut connection_state) = self.connection_state.write() {
                    connection_state.allow_new_peers();
                }
            }

            false
        } else if self.reserved_connected_peers.remove(&peer_id).is_some() {
            self.send_reserved_peers_update();
            true
        } else {
            false
        }
    }

    /// Find a peer that is holding the given block height.
    pub fn get_peer_id_with_height(&self, height: &BlockHeight) -> Option<PeerId> {
        let mut range = rand::thread_rng();
        // TODO: Optimize the selection of the peer.
        //  We can store pair `(peer id, height)` for all nodes(reserved and not) in the
        //  https://docs.rs/sorted-vec/latest/sorted_vec/struct.SortedVec.html
        self.non_reserved_connected_peers
            .iter()
            .chain(self.reserved_connected_peers.iter())
            .filter(|(_, peer_info)| {
                peer_info.heartbeat_data.block_height >= Some(*height)
            })
            .map(|(peer_id, _)| *peer_id)
            .choose(&mut range)
    }

    /// Handles the first connnection established with a Peer    
    fn handle_initial_connection(
        &mut self,
        peer_id: &PeerId,
        addresses: Vec<Multiaddr>,
    ) -> bool {
        // if the connected Peer is not from the reserved peers
        if !self.reserved_peers.contains(peer_id) {
            let non_reserved_peers_connected = self.non_reserved_connected_peers.len();
            // check if all the slots are already taken
            if non_reserved_peers_connected >= self.max_non_reserved_peers {
                // Too many peers already connected, disconnect the Peer
                return true
            }

            if non_reserved_peers_connected + 1 == self.max_non_reserved_peers {
                // this is the last non-reserved peer allowed
                if let Ok(mut connection_state) = self.connection_state.write() {
                    connection_state.deny_new_peers();
                }
            }

            self.non_reserved_connected_peers
                .insert(*peer_id, PeerInfo::default());
        } else {
            self.reserved_connected_peers
                .insert(*peer_id, PeerInfo::default());

            self.send_reserved_peers_update();
        }

        self.insert_peer_info(peer_id, PeerInfoInsert::Addresses(addresses));

        false
    }

    fn send_reserved_peers_update(&self) {
        let _ = self
            .reserved_peers_updates
            .send(self.reserved_connected_peers.len());
    }

    fn insert_peer_info(&mut self, peer_id: &PeerId, data: PeerInfoInsert) {
        let peers = if self.reserved_peers.contains(peer_id) {
            &mut self.reserved_connected_peers
        } else {
            &mut self.non_reserved_connected_peers
        };
        match data {
            PeerInfoInsert::Addresses(addresses) => {
                insert_peer_addresses(peers, peer_id, addresses)
            }
            PeerInfoInsert::ClientVersion(client_version) => {
                insert_client_version(peers, peer_id, client_version)
            }
            PeerInfoInsert::HeartbeatData(block_height) => {
                insert_heartbeat_data(peers, peer_id, block_height)
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ConnectionState {
    peers_allowed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct HeartbeatData {
    pub block_height: Option<BlockHeight>,
    pub last_heartbeat: Option<Instant>,
}

impl HeartbeatData {
    pub fn new(block_height: BlockHeight) -> Self {
        Self {
            block_height: Some(block_height),
            last_heartbeat: Some(Instant::now()),
        }
    }

    pub fn seconds_since_last_heartbeat(&self) -> Option<Duration> {
        self.last_heartbeat.map(|time| time.elapsed())
    }
}

impl ConnectionState {
    pub fn new() -> Arc<RwLock<Self>> {
        Arc::new(RwLock::new(Self {
            peers_allowed: true,
        }))
    }

    pub fn available_slot(&self) -> bool {
        self.peers_allowed
    }

    fn allow_new_peers(&mut self) {
        self.peers_allowed = true;
    }

    fn deny_new_peers(&mut self) {
        self.peers_allowed = false;
    }
}

fn insert_peer_addresses(
    peers: &mut HashMap<PeerId, PeerInfo>,
    peer_id: &PeerId,
    addresses: Vec<Multiaddr>,
) {
    if let Some(peer) = peers.get_mut(peer_id) {
        for address in addresses {
            peer.peer_addresses.insert(address);
        }
    } else {
        log_missing_peer(peer_id);
    }
}

fn insert_heartbeat_data(
    peers: &mut HashMap<PeerId, PeerInfo>,
    peer_id: &PeerId,
    heartbeat_data: HeartbeatData,
) {
    if let Some(peer) = peers.get_mut(peer_id) {
        peer.heartbeat_data = heartbeat_data;
    } else {
        log_missing_peer(peer_id);
    }
}

fn insert_client_version(
    peers: &mut HashMap<PeerId, PeerInfo>,
    peer_id: &PeerId,
    client_version: String,
) {
    if let Some(peer) = peers.get_mut(peer_id) {
        peer.client_version = Some(client_version);
    } else {
        log_missing_peer(peer_id);
    }
}

fn log_missing_peer(peer_id: &PeerId) {
    debug!(target: "fuel-p2p", "Peer with PeerId: {:?} is not among the connected peers", peer_id)
}

#[derive(Clone, Debug, Copy)]
struct ScoreConfig {
    max_app_score: AppScore,
    min_app_score_allowed: AppScore,
    min_gossip_score_allowed: f64,
}

impl Default for ScoreConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl ScoreConfig {
    pub fn new() -> Self {
        Self {
            max_app_score: MAX_APP_SCORE,
            min_app_score_allowed: MIN_APP_SCORE,
            min_gossip_score_allowed: MIN_GOSSIPSUB_SCORE_BEFORE_BAN,
        }
    }
}

pub trait Punisher {
    fn ban_peer(&mut self, peer_id: PeerId);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_random_peers(size: usize) -> Vec<PeerId> {
        (0..size).map(|_| PeerId::random()).collect()
    }

    fn initialize_peer_manager(
        reserved_peers: Vec<PeerId>,
        max_non_reserved_peers: usize,
    ) -> PeerManager {
        let connection_state = ConnectionState::new();

        PeerManager::new(
            reserved_peers.into_iter().collect(),
            connection_state,
            max_non_reserved_peers,
        )
    }

    #[test]
    fn only_allowed_number_of_non_reserved_peers_is_connected() {
        let max_non_reserved_peers = 5;
        let mut peer_manager = initialize_peer_manager(vec![], max_non_reserved_peers);

        let random_peers = get_random_peers(max_non_reserved_peers * 2);

        // try connecting all the random peers
        for peer_id in &random_peers {
            peer_manager.handle_initial_connection(peer_id, vec![]);
        }

        assert_eq!(peer_manager.total_peers_connected(), max_non_reserved_peers);
    }

    #[test]
    fn only_reserved_peers_are_connected() {
        let max_non_reserved_peers = 0;
        let reserved_peers = get_random_peers(5);
        let mut peer_manager =
            initialize_peer_manager(reserved_peers.clone(), max_non_reserved_peers);

        // try connecting all the reserved peers
        for peer_id in &reserved_peers {
            peer_manager.handle_initial_connection(peer_id, vec![]);
        }

        assert_eq!(peer_manager.total_peers_connected(), reserved_peers.len());

        // try connecting random peers
        let random_peers = get_random_peers(10);
        for peer_id in &random_peers {
            peer_manager.handle_initial_connection(peer_id, vec![]);
        }

        // the number should stay the same
        assert_eq!(peer_manager.total_peers_connected(), reserved_peers.len());
    }

    #[test]
    fn non_reserved_peer_does_not_take_reserved_slot() {
        let max_non_reserved_peers = 5;
        let reserved_peers = get_random_peers(5);
        let mut peer_manager =
            initialize_peer_manager(reserved_peers.clone(), max_non_reserved_peers);

        // try connecting all the reserved peers
        for peer_id in &reserved_peers {
            peer_manager.handle_initial_connection(peer_id, vec![]);
        }

        // disconnect a single reserved peer
        peer_manager.handle_peer_disconnect(*reserved_peers.first().unwrap());

        // try connecting random peers
        let random_peers = get_random_peers(max_non_reserved_peers * 2);
        for peer_id in &random_peers {
            peer_manager.handle_initial_connection(peer_id, vec![]);
        }

        // there should be an available slot for a reserved peer
        assert_eq!(
            peer_manager.total_peers_connected(),
            reserved_peers.len() - 1 + max_non_reserved_peers
        );

        // reconnect the disconnected reserved peer
        peer_manager.handle_initial_connection(reserved_peers.first().unwrap(), vec![]);

        // all the slots should be taken now
        assert_eq!(
            peer_manager.total_peers_connected(),
            reserved_peers.len() + max_non_reserved_peers
        );
    }
}
