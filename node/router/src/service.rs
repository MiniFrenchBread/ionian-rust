use crate::peer_manager::PeerManager;
use crate::Config;
use file_location_cache::FileLocationCache;
use futures::{channel::mpsc::Sender, prelude::*};
use miner::MinerMessage;
use network::{
    rpc::StatusMessage,
    types::{AnnounceFile, FindFile, SignedAnnounceFile},
    BehaviourEvent, Keypair, Libp2pEvent, MessageAcceptance, MessageId, NetworkGlobals,
    NetworkMessage, PeerId, PeerRequestId, PublicKey, PubsubMessage, Request, RequestId, Response,
    Service as LibP2PService, Swarm,
};
use shared_types::timestamp_now;
use std::time::Duration;
use std::{ops::Neg, sync::Arc};
use storage::log_store::Store as LogStore;
use storage_async::Store;
use sync::{SyncMessage, SyncSender};
use task_executor::ShutdownReason;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::time::interval;

pub fn peer_id_to_public_key(peer_id: &PeerId) -> Result<PublicKey, String> {
    // A libp2p peer id byte representation should be 2 length bytes + 4 protobuf bytes + compressed pk bytes
    // if generated from a PublicKey with Identity multihash.
    let pk_bytes = &peer_id.to_bytes()[2..];

    PublicKey::from_protobuf_encoding(pk_bytes).map_err(|e| {
        format!(
            " Cannot parse libp2p public key public key from peer id: {}",
            e
        )
    })
}

fn duration_since(timestamp: u32) -> chrono::Duration {
    let timestamp = i64::try_from(timestamp).expect("Should fit");
    let timestamp = chrono::NaiveDateTime::from_timestamp(timestamp, 0);
    let now = chrono::Utc::now().naive_utc();
    now.signed_duration_since(timestamp)
}

lazy_static::lazy_static! {
    pub static ref FIND_FILE_TIMEOUT: chrono::Duration = chrono::Duration::minutes(2);
    pub static ref ANNOUNCE_FILE_TIMEOUT: chrono::Duration = chrono::Duration::minutes(2);
    pub static ref TOLERABLE_DRIFT: chrono::Duration = chrono::Duration::seconds(5);
}

/// Service that handles communication between internal services and the libp2p service.
pub struct RouterService {
    config: Config,

    /// The underlying libp2p service that drives all the network interactions.
    libp2p: LibP2PService<RequestId>,

    /// A collection of global variables, accessible outside of the network service.
    network_globals: Arc<NetworkGlobals>,

    /// The receiver channel for Ionian to communicate with the network service.
    network_recv: mpsc::UnboundedReceiver<NetworkMessage>,

    /// A channel to the router service.
    network_send: mpsc::UnboundedSender<NetworkMessage>,

    /// A channel to the syncing service.
    sync_send: SyncSender,

    /// A channel to the miner service.
    #[allow(dead_code)]
    miner_send: Option<broadcast::Sender<MinerMessage>>,

    /// Log and transaction storage.
    store: Store,

    /// Cache for storing and serving gossip messages.
    file_location_cache: Arc<FileLocationCache>,

    /// Node keypair for signing messages.
    local_keypair: Keypair,

    /// All connected peers.
    peers: PeerManager,
}

impl RouterService {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        executor: task_executor::TaskExecutor,
        libp2p: LibP2PService<RequestId>,
        network_globals: Arc<NetworkGlobals>,
        network_recv: mpsc::UnboundedReceiver<NetworkMessage>,
        network_send: mpsc::UnboundedSender<NetworkMessage>,
        sync_send: SyncSender,
        miner_send: Option<broadcast::Sender<MinerMessage>>,
        store: Arc<RwLock<dyn LogStore>>,
        file_location_cache: Arc<FileLocationCache>,
        local_keypair: Keypair,
        config: Config,
    ) {
        let store = Store::new(store, executor.clone());

        // create the network service and spawn the task
        let router = RouterService {
            config: config.clone(),
            libp2p,
            network_globals,
            network_recv,
            network_send,
            sync_send,
            miner_send,
            store,
            file_location_cache,
            local_keypair,
            peers: PeerManager::new(config),
        };

        // spawn service
        let shutdown_sender = executor.shutdown_sender();

        executor.spawn(
            async move { Box::pin(router.main(shutdown_sender)).await },
            "router",
        );
    }

    async fn main(mut self, mut shutdown_sender: Sender<ShutdownReason>) {
        let mut heartbeat = interval(Duration::from_secs(self.config.heartbeat_interval_secs));

        loop {
            tokio::select! {
                // handle a message sent to the network
                Some(msg) = self.network_recv.recv() => self.on_network_msg(msg, &mut shutdown_sender).await,

                // handle event coming from the network
                event = self.libp2p.next_event() => self.on_libp2p_event(event, &mut shutdown_sender).await,

                // heartbeat
                _ = heartbeat.tick() => self.on_heartbeat(),
            }
        }
    }

    fn send_to_sync(&mut self, message: SyncMessage) {
        self.sync_send.notify(message).unwrap_or_else(|e| {
            warn!( error = %e, "Could not send message to the sync service");
        });
    }

    fn send_to_network(&mut self, message: NetworkMessage) {
        self.network_send.send(message).unwrap_or_else(|e| {
            warn!( error = %e, "Could not send message to the network service");
        });
    }

    fn publish(&mut self, msg: PubsubMessage) {
        self.send_to_network(NetworkMessage::Publish {
            messages: vec![msg],
        });
    }

    /// Handle an event received from the network.
    async fn on_libp2p_event(
        &mut self,
        ev: Libp2pEvent<RequestId>,
        shutdown_sender: &mut Sender<ShutdownReason>,
    ) {
        debug!(?ev, "Received new event from libp2p");

        match ev {
            Libp2pEvent::Behaviour(event) => match event {
                BehaviourEvent::PeerConnectedOutgoing(peer_id) => {
                    self.on_peer_connected(peer_id, true);
                }
                BehaviourEvent::PeerConnectedIncoming(peer_id) => {
                    self.on_peer_connected(peer_id, false);
                }
                BehaviourEvent::PeerBanned(_) | BehaviourEvent::PeerUnbanned(_) => {
                    // No action required for these events.
                }
                BehaviourEvent::PeerDisconnected(peer_id) => {
                    self.on_peer_disconnected(peer_id);
                }
                BehaviourEvent::RequestReceived {
                    peer_id,
                    id,
                    request,
                } => {
                    self.on_rpc_request(peer_id, id, request);
                }
                BehaviourEvent::ResponseReceived {
                    peer_id,
                    id,
                    response,
                } => {
                    self.on_rpc_response(peer_id, id, response);
                }
                BehaviourEvent::RPCFailed { id, peer_id } => {
                    self.on_rpc_error(peer_id, id);
                }
                BehaviourEvent::StatusPeer(peer_id) => {
                    self.send_status(peer_id);
                }
                BehaviourEvent::PubsubMessage {
                    id,
                    propagation_source,
                    source,
                    message,
                    ..
                } => {
                    self.on_pubsub_message(propagation_source, source, id, message)
                        .await;
                }
            },
            Libp2pEvent::NewListenAddr(multiaddr) => {
                self.network_globals
                    .listen_multiaddrs
                    .write()
                    .push(multiaddr);
            }
            Libp2pEvent::ZeroListeners => {
                let _ = shutdown_sender
                    .send(ShutdownReason::Failure(
                        "All listeners are closed. Unable to listen",
                    ))
                    .await
                    .map_err(|e| {
                        warn!(
                            error = %e,
                            "failed to send a shutdown signal",
                        )
                    });
            }
        }
    }

    /// Handle a message sent to the network service.
    async fn on_network_msg(
        &mut self,
        msg: NetworkMessage,
        _shutdown_sender: &mut Sender<ShutdownReason>,
    ) {
        debug!(?msg, "Received new message");

        match msg {
            NetworkMessage::SendRequest {
                peer_id,
                request,
                request_id,
            } => {
                self.libp2p.send_request(peer_id, request_id, request);
            }
            NetworkMessage::SendResponse {
                peer_id,
                response,
                id,
            } => {
                self.libp2p.send_response(peer_id, id, response);
            }
            NetworkMessage::SendErrorResponse {
                peer_id,
                error,
                id,
                reason,
            } => {
                self.libp2p.respond_with_error(peer_id, id, error, reason);
            }
            NetworkMessage::Publish { messages } => {
                if self.libp2p.swarm.connected_peers().next().is_none() {
                    // this is a boardcast message, when current node doesn't have any peers connected, try to connect any peer in config
                    for multiaddr in &self.config.libp2p_nodes {
                        match Swarm::dial(&mut self.libp2p.swarm, multiaddr.clone()) {
                            Ok(()) => {
                                debug!(address = %multiaddr, "Dialing libp2p peer");
                                break;
                            }
                            Err(err) => {
                                debug!(address = %multiaddr, error = ?err, "Could not connect to peer")
                            }
                        };
                    }
                }

                let mut topic_kinds = Vec::new();
                for message in &messages {
                    if !topic_kinds.contains(&message.kind()) {
                        topic_kinds.push(message.kind());
                    }
                }
                debug!(
                    count = messages.len(),
                    topics = ?topic_kinds,
                    "Sending pubsub messages",
                );
                self.libp2p.swarm.behaviour_mut().publish(messages);
            }
            NetworkMessage::ReportPeer {
                peer_id,
                action,
                source,
                msg,
            } => self.libp2p.report_peer(&peer_id, action, source, msg),
            NetworkMessage::GoodbyePeer {
                peer_id,
                reason,
                source,
            } => self.libp2p.goodbye_peer(&peer_id, reason, source),
            NetworkMessage::DialPeer { address, peer_id } => {
                if self.libp2p.swarm.is_connected(&peer_id) {
                    self.send_to_sync(SyncMessage::PeerConnected { peer_id });
                } else {
                    match Swarm::dial(&mut self.libp2p.swarm, address.clone()) {
                        Ok(()) => debug!(%address, "Dialing libp2p peer"),
                        Err(err) => {
                            info!(%address, error = ?err, "Failed to dial peer");
                            self.send_to_sync(SyncMessage::DailFailed { peer_id, err });
                        }
                    };
                }
            }
            NetworkMessage::AnnounceLocalFile { tx_seq } => {
                if let Some(msg) = self.construct_announce_file_message(tx_seq) {
                    self.publish(msg);
                }
            }
        }
    }

    fn on_peer_connected(&mut self, peer_id: PeerId, outgoing: bool) {
        self.peers.add(peer_id, outgoing);

        if outgoing {
            self.send_status(peer_id);
            self.send_to_sync(SyncMessage::PeerConnected { peer_id });
        }
    }

    fn on_peer_disconnected(&mut self, peer_id: PeerId) {
        self.peers.remove(&peer_id);
        self.send_to_sync(SyncMessage::PeerDisconnected { peer_id });
    }

    fn on_rpc_request(&mut self, peer_id: PeerId, request_id: PeerRequestId, request: Request) {
        if !self.network_globals.peers.read().is_connected(&peer_id) {
            debug!(%peer_id, ?request, "Dropping request of disconnected peer");
            return;
        }

        self.peers.update(&peer_id);

        match request {
            Request::Status(status) => {
                self.on_status_request(peer_id, request_id, status);
            }
            Request::GetChunks(request) => {
                self.send_to_sync(SyncMessage::RequestChunks {
                    peer_id,
                    request_id,
                    request,
                });
            }
            Request::DataByHash(_) => {
                // ignore
            }
        }
    }

    fn on_rpc_response(&mut self, peer_id: PeerId, request_id: RequestId, response: Response) {
        self.peers.update(&peer_id);

        match response {
            Response::Status(status_message) => {
                self.on_status_response(peer_id, status_message);
            }
            Response::Chunks(response) => {
                let request_id = match request_id {
                    RequestId::Sync(sync_id) => sync_id,
                    _ => unreachable!("All Chunks responses belong to sync"),
                };

                self.send_to_sync(SyncMessage::ChunksResponse {
                    peer_id,
                    request_id,
                    response,
                });
            }
            Response::DataByHash(_) => {
                // ignore
            }
        }
    }

    fn on_rpc_error(&mut self, peer_id: PeerId, request_id: RequestId) {
        self.peers.update(&peer_id);

        // Check if the failed RPC belongs to sync
        if let RequestId::Sync(request_id) = request_id {
            self.send_to_sync(SyncMessage::RpcError {
                peer_id,
                request_id,
            });
        }
    }

    fn send_status(&mut self, peer_id: PeerId) {
        let status_message = StatusMessage { data: 123 }; // dummy status message
        debug!(%peer_id, ?status_message, "Sending Status request");

        self.send_to_network(NetworkMessage::SendRequest {
            peer_id,
            request_id: RequestId::Router,
            request: Request::Status(status_message),
        })
    }

    fn on_status_request(
        &mut self,
        peer_id: PeerId,
        request_id: PeerRequestId,
        status: StatusMessage,
    ) {
        debug!(%peer_id, ?status, "Received Status request");

        let status_message = StatusMessage { data: 456 }; // dummy status message
        debug!(%peer_id, ?status_message, "Sending Status response");

        self.send_to_network(NetworkMessage::SendResponse {
            peer_id,
            id: request_id,
            response: Response::Status(status_message),
        });
    }

    pub fn on_status_response(&mut self, peer_id: PeerId, status: StatusMessage) {
        debug!(%peer_id, ?status, "Received Status response");
    }

    async fn on_pubsub_message(
        &mut self,
        propagation_source: PeerId,
        source: PeerId,
        id: MessageId,
        message: PubsubMessage,
    ) {
        info!(?message, %propagation_source, %source, %id, "Received pubsub message");

        let result = match message {
            PubsubMessage::ExampleMessage(_) => MessageAcceptance::Ignore,
            PubsubMessage::FindFile(msg) => self.on_find_file(msg).await,
            PubsubMessage::AnnounceFile(msg) => self.on_announce_file(propagation_source, msg),
        };

        self.libp2p
            .swarm
            .behaviour_mut()
            .report_message_validation_result(&propagation_source, id, result);
    }

    fn construct_announce_file_message(&self, tx_seq: u64) -> Option<PubsubMessage> {
        let peer_id = *self.network_globals.peer_id.read();

        let addr = match self.network_globals.listen_multiaddrs.read().first() {
            Some(addr) => addr.clone(),
            None => {
                error!("No listen address available");
                return None;
            }
        };

        let timestamp = timestamp_now();

        let msg = AnnounceFile {
            tx_seq,
            peer_id: peer_id.into(),
            at: addr.into(),
            timestamp,
        };

        let mut signed = match msg.into_signed(&self.local_keypair) {
            Ok(signed) => signed,
            Err(e) => {
                error!(%tx_seq, %e, "Failed to sign AnnounceFile message");
                return None;
            }
        };

        signed.resend_timestamp = timestamp;

        Some(PubsubMessage::AnnounceFile(signed))
    }

    async fn on_find_file(&mut self, msg: FindFile) -> MessageAcceptance {
        let FindFile { tx_seq, timestamp } = msg;

        // verify timestamp
        let d = duration_since(timestamp);
        if d < TOLERABLE_DRIFT.neg() || d > *FIND_FILE_TIMEOUT {
            debug!(%timestamp, "Invalid timestamp, ignoring FindFile message");
            return MessageAcceptance::Ignore;
        }

        // check if we have it
        if matches!(self.store.check_tx_completed(tx_seq).await, Ok(true)) {
            debug!(%tx_seq, "Found file locally, responding to FindFile query");

            return match self.construct_announce_file_message(tx_seq) {
                Some(msg) => {
                    self.publish(msg);
                    MessageAcceptance::Ignore
                }
                // propagate FindFile query to other nodes
                None => MessageAcceptance::Accept,
            };
        }

        // try from cache
        if let Some(mut msg) = self.file_location_cache.get_one(tx_seq) {
            debug!(%tx_seq, "Found file in cache, responding to FindFile query");

            msg.resend_timestamp = timestamp_now();
            self.publish(PubsubMessage::AnnounceFile(msg));

            return MessageAcceptance::Ignore;
        }

        // propagate FindFile query to other nodes
        MessageAcceptance::Accept
    }

    fn on_announce_file(
        &mut self,
        propagation_source: PeerId,
        msg: SignedAnnounceFile,
    ) -> MessageAcceptance {
        // verify message signature
        let pk = match peer_id_to_public_key(&msg.peer_id) {
            Ok(pk) => pk,
            Err(e) => {
                error!(
                    "Failed to convert peer id {:?} to public key: {:?}",
                    msg.peer_id, e
                );
                return MessageAcceptance::Reject;
            }
        };

        if !msg.verify_signature(&pk) {
            warn!(
                "Received message with invalid signature from peer {:?}",
                propagation_source
            );
            return MessageAcceptance::Reject;
        }

        // propagate gossip to peers
        let d = duration_since(msg.resend_timestamp);
        if d < TOLERABLE_DRIFT.neg() || d > *ANNOUNCE_FILE_TIMEOUT {
            debug!(%msg.resend_timestamp, "Invalid resend timestamp, ignoring AnnounceFile message");
            return MessageAcceptance::Ignore;
        }

        // notify sync layer
        self.send_to_sync(SyncMessage::AnnounceFileGossip {
            tx_seq: msg.tx_seq,
            peer_id: msg.peer_id.clone().into(),
            addr: msg.at.clone().into(),
        });

        // insert message to cache
        self.file_location_cache.insert(msg);

        MessageAcceptance::Accept
    }

    fn on_heartbeat(&mut self) {
        let expired_peers = self.peers.expired_peers();

        trace!("heartbeat, expired peers = {:?}", expired_peers.len());

        for peer_id in expired_peers {
            // async operation, once peer disconnected, swarm event `PeerDisconnected`
            // will be polled to handle in advance.
            match self.libp2p.swarm.disconnect_peer_id(peer_id) {
                Ok(_) => debug!(%peer_id, "Peer expired and disconnect it"),
                Err(_) => error!(%peer_id, "Peer expired but failed to disconnect"),
            }
        }
    }
}

impl Drop for RouterService {
    fn drop(&mut self) {
        info!("Router service shutdown");
    }
}
