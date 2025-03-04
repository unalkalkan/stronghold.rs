// Copyright 2020-2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use super::{connections::ConnectionManager, *};
use crate::behaviour::{
    BehaviourError, MessageEvent, P2PEvent, P2PNetworkBehaviour, P2POutboundFailure, P2PReqResEvent, RequestEnvelope,
};
use core::{ops::Deref, str::FromStr, time::Duration};
use futures::{channel::mpsc::UnboundedReceiver, future, prelude::*, select};
use libp2p::{
    core::{connection::ListenerId, multiaddr::Protocol, ConnectedPoint},
    identity::Keypair,
    request_response::RequestId,
    swarm::{DialError, Swarm, SwarmEvent},
    Multiaddr, PeerId,
};
use riker::{actors::*, Message};
use std::{
    net::Ipv4Addr,
    task::{Context, Poll},
    time::Instant,
};

// Separate task that manages the swarm communication.
pub(super) struct SwarmTask<Req, Res, ClientMsg, P>
where
    Req: MessageEvent + ToPermissionVariants<P> + Into<ClientMsg>,
    Res: MessageEvent,
    ClientMsg: Message,
    P: Message + VariantPermission,
{
    system: ActorSystem,
    // client to receive incoming requests
    client: ActorRef<ClientMsg>,
    // firewall configuration to check and validate all outgoing and incoming requests
    firewall: FirewallConfiguration,
    // the expanded swarm that is used to poll for incoming requests and interact
    swarm: Swarm<P2PNetworkBehaviour<RequestEnvelope<Req>, Res>>,
    // channel from the communication actor to this task
    swarm_rx: UnboundedReceiver<(CommunicationRequest<Req, ClientMsg>, Sender)>,
    // current listener in the swarm
    listener: Option<ListenerId>,
    // configuration to use optionally use a relay peer if a peer in a remote network can not be reached directly.
    relay: RelayConfig,
    // maintain the current state of connections and keep-alive configuration
    connection_manager: ConnectionManager,
    _marker: PhantomData<P>,
}

impl<Req, Res, ClientMsg, P> SwarmTask<Req, Res, ClientMsg, P>
where
    Req: MessageEvent + ToPermissionVariants<P> + Into<ClientMsg>,
    Res: MessageEvent,
    ClientMsg: Message,
    P: Message + VariantPermission,
{
    pub async fn new(
        system: ActorSystem,
        swarm_rx: UnboundedReceiver<(CommunicationRequest<Req, ClientMsg>, Sender)>,
        actor_config: CommunicationActorConfig<ClientMsg>,
        keypair: Keypair,
        behaviour: BehaviourConfig,
    ) -> Result<Self, BehaviourError> {
        // Create a P2PNetworkBehaviour for the swarm communication.
        let swarm = P2PNetworkBehaviour::<RequestEnvelope<Req>, Res>::init_swarm(keypair, behaviour).await?;
        let firewall = FirewallConfiguration::new(actor_config.firewall_default_in, actor_config.firewall_default_out);
        Ok(SwarmTask {
            system,
            client: actor_config.client,
            firewall,
            swarm,
            swarm_rx,
            listener: None,
            relay: RelayConfig::NoRelay,
            connection_manager: ConnectionManager::new(),
            _marker: PhantomData,
        })
    }

    // Poll from the swarm for events from remote peers, and from the `swarm_tx` channel for events from the local
    // actor, and forward them.
    pub async fn poll_swarm(mut self) {
        loop {
            select! {
                swarm_event = self.swarm.next_event().fuse() => self.handle_swarm_event(swarm_event),
                actor_event = self.swarm_rx.next().fuse() => {
                    if let Some((message, sender)) = actor_event {
                        if let CommunicationRequest::Shutdown = message {
                            break;
                        } else {
                            self.handle_actor_request(message, sender)
                        }
                    } else {
                        break
                    }
                },
            };
        }
        self.shutdown();
    }

    fn shutdown(mut self) {
        if let Some(listener_id) = self.listener.take() {
            let _ = Swarm::remove_listener(&mut self.swarm, listener_id);
        }
        self.swarm_rx.close();
    }

    // Send a reponse to the sender of a previous [`CommunicationRequest`]
    fn send_response(result: CommunicationResults<Res>, sender: Sender) {
        if let Some(sender) = sender {
            let _ = sender.try_tell(result, None);
        }
    }

    // Forward request to client actor and wait for the result, with 3s timeout.
    fn ask_client(&mut self, request: Req) -> Option<Res> {
        let start = Instant::now();
        let mut ask_client = ask(&self.system, &self.client, request);
        task::block_on(future::poll_fn(move |cx: &mut Context<'_>| {
            match ask_client.poll_unpin(cx) {
                Poll::Ready(res) => Poll::Ready(Some(res)),
                Poll::Pending => {
                    if start.elapsed() > Duration::new(3, 0) {
                        Poll::Ready(None)
                    } else {
                        Poll::Pending
                    }
                }
            }
        }))
    }

    // Start listening on the swarm, if not address is provided, the port will be OS assigned.
    fn start_listening(&mut self, addr: Option<Multiaddr>) -> Result<Multiaddr, ()> {
        let addr = addr.unwrap_or_else(|| {
            Multiaddr::empty()
                .with(Protocol::Ip4(Ipv4Addr::new(0, 0, 0, 0)))
                .with(Protocol::Tcp(0u16))
        });
        if let Ok(listener_id) = Swarm::listen_on(&mut self.swarm, addr) {
            let start = Instant::now();
            task::block_on(async {
                loop {
                    match self.swarm.next_event().await {
                        SwarmEvent::NewListenAddr(addr) => {
                            self.listener = Some(listener_id);
                            return Ok(addr);
                        }
                        other => self.handle_swarm_event(other),
                    }
                    if start.elapsed() > Duration::new(3, 0) {
                        return Err(());
                    }
                }
            })
        } else {
            Err(())
        }
    }

    // Try to connect a remote peer by id, and if the peer id is not know yet the address is used.
    fn connect_peer(&mut self, target_peer: PeerId, target_addr: Multiaddr) -> Result<PeerId, ConnectPeerError> {
        if let Err(err) = Swarm::dial(&mut self.swarm, &target_peer) {
            match err {
                DialError::NoAddresses => {
                    if let Err(err) = Swarm::dial_addr(&mut self.swarm, target_addr.clone()) {
                        return Err(err.into());
                    }
                }
                _ => {
                    return Err(err.into());
                }
            }
        }
        let start = Instant::now();
        task::block_on(async {
            loop {
                let event = self.swarm.next_event().await;
                match event {
                    SwarmEvent::ConnectionEstablished {
                        peer_id,
                        endpoint: ConnectedPoint::Dialer { address: _ },
                        num_established: _,
                    } => {
                        if peer_id == target_peer {
                            return Ok(peer_id);
                        } else {
                            self.handle_swarm_event(event)
                        }
                    }
                    SwarmEvent::UnreachableAddr {
                        peer_id,
                        address: _,
                        error,
                        attempts_remaining: 0,
                    } => {
                        if peer_id == target_peer {
                            return Err(ConnectPeerError::from(error));
                        }
                    }
                    SwarmEvent::UnknownPeerUnreachableAddr { address, error } => {
                        if address == target_addr {
                            return Err(ConnectPeerError::from(error));
                        }
                    }
                    _ => self.handle_swarm_event(event),
                }
                if start.elapsed() > Duration::new(3, 0) {
                    return Err(ConnectPeerError::Timeout);
                }
            }
        })
    }

    // Try sending a request envelope to a remote peer if it was approved by the firewall, and return the received
    // Response. If no response is received, a RequestMessageError::Rejected will be returned.
    fn send_envelope_to_peer(
        &mut self,
        peer_id: PeerId,
        envelope: RequestEnvelope<Req>,
    ) -> Result<Res, RequestMessageError> {
        let req_id = self.swarm.send_request(&peer_id, envelope);
        let start = Instant::now();
        task::block_on(async {
            loop {
                let event = self.swarm.next_event().await;
                match event {
                    SwarmEvent::Behaviour(P2PEvent::RequestResponse(ref boxed_event)) => {
                        match boxed_event.clone().deref().clone() {
                            P2PReqResEvent::Res {
                                peer_id: _,
                                request_id,
                                response,
                            } => {
                                if request_id == req_id {
                                    return Ok(response);
                                }
                            }
                            P2PReqResEvent::InboundFailure {
                                peer_id: _,
                                request_id,
                                error,
                            } => {
                                if request_id == req_id {
                                    return Err(RequestMessageError::Inbound(error));
                                }
                            }
                            P2PReqResEvent::OutboundFailure {
                                peer_id: _,
                                request_id,
                                error,
                            } => {
                                if request_id == req_id {
                                    return Err(RequestMessageError::Outbound(error));
                                }
                            }
                            _ => self.handle_swarm_event(event),
                        }
                    }
                    _ => self.handle_swarm_event(event),
                }
                if start.elapsed() > Duration::new(3, 0) {
                    return Err(RequestMessageError::Rejected(FirewallBlocked::Remote));
                }
            }
        })
    }

    // Wrap the request into an envelope, which enables using a relay peer, and send it to the remote.
    // Depending on the config, it is ether send directly or via the relay.
    fn send_request(&mut self, peer_id: PeerId, request: Req) -> Result<Res, RequestMessageError> {
        let local_peer = Swarm::local_peer_id(&self.swarm);
        let envelope = RequestEnvelope {
            source: local_peer.to_string(),
            message: request,
            target: peer_id.to_string(),
        };
        match self.relay {
            RelayConfig::NoRelay => self.send_envelope_to_peer(peer_id, envelope),
            RelayConfig::RelayAlways {
                peer_id: relay_id,
                addr: _,
            } => self.send_envelope_to_peer(relay_id, envelope),
            RelayConfig::RelayBackup {
                peer_id: relay_id,
                addr: _,
            } => {
                // try sending directly, otherwise use relay
                let res = self.send_envelope_to_peer(peer_id, envelope.clone());
                if let Err(RequestMessageError::Outbound(P2POutboundFailure::DialFailure)) = res {
                    self.send_envelope_to_peer(relay_id, envelope)
                } else {
                    res
                }
            }
        }
    }

    // Set the new relay configuration. If a relay is use, a keep-alive connection to the relay will be established.
    fn set_relay(&mut self, config: RelayConfig) -> Result<(), ConnectPeerError> {
        match config.clone() {
            RelayConfig::NoRelay => Ok(()),
            RelayConfig::RelayAlways { peer_id, addr } | RelayConfig::RelayBackup { peer_id, addr } => {
                let res = self.connect_peer(peer_id, addr.clone());
                match res {
                    Ok(_) => {
                        let endpoint = ConnectedPoint::Dialer { address: addr };
                        self.connection_manager.insert(peer_id, endpoint, KeepAlive::Unlimited);
                        self.relay = config;
                        Ok(())
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    fn configure_firewall(&mut self, rule: FirewallRule) {
        match rule {
            FirewallRule::SetRules {
                direction,
                peers,
                set_default,
                permission,
            } => {
                for peer in peers {
                    self.firewall.set_rule(peer, &direction, permission);
                }
                if set_default {
                    self.firewall.set_default(&direction, permission);
                }
            }
            FirewallRule::AddPermissions {
                direction,
                peers,
                change_default,
                permissions,
            } => {
                for peer in peers {
                    let init = self
                        .firewall
                        .get_rule(&peer, &direction)
                        .unwrap_or_else(|| self.firewall.get_default(&direction));
                    let rule = permissions.iter().fold(init, |acc, curr| acc.add_permission(curr));
                    self.firewall.set_rule(peer, &direction, rule);
                }
                if change_default {
                    let init = self.firewall.get_default(&direction);
                    let new_default = permissions.iter().fold(init, |acc, curr| acc.add_permission(curr));
                    self.firewall.set_default(&direction, new_default);
                }
            }
            FirewallRule::RemovePermissions {
                direction,
                peers,
                change_default,
                permissions,
            } => {
                for peer in peers {
                    let init = self
                        .firewall
                        .get_rule(&peer, &direction)
                        .unwrap_or_else(|| self.firewall.get_default(&direction));
                    let rule = permissions.iter().fold(init, |acc, curr| acc.remove_permission(curr));
                    self.firewall.set_rule(peer, &direction, rule);
                }
                if change_default {
                    let init = self.firewall.get_default(&direction);
                    let new_default = permissions.iter().fold(init, |acc, curr| acc.remove_permission(&curr));
                    self.firewall.set_default(&direction, new_default)
                }
            }
            FirewallRule::RemoveRule { peers, direction } => {
                for peer in peers {
                    self.firewall.remove_rule(&peer, &direction);
                }
            }
        }
    }

    // Handle the messages that are received from other actors in the system.
    fn handle_actor_request(&mut self, event: CommunicationRequest<Req, ClientMsg>, sender: Sender) {
        match event {
            CommunicationRequest::RequestMsg { peer_id, request } => {
                let res = if self
                    .firewall
                    .is_permitted(request.clone(), peer_id, RequestDirection::Out)
                {
                    self.send_request(peer_id, request)
                } else {
                    Err(RequestMessageError::Rejected(FirewallBlocked::Local))
                };
                Self::send_response(CommunicationResults::RequestMsgResult(res), sender);
            }
            CommunicationRequest::SetClientRef(client_ref) => {
                self.client = client_ref;
                let res = CommunicationResults::SetClientRefAck;
                Self::send_response(res, sender);
            }
            CommunicationRequest::EstablishConnection {
                peer_id,
                addr,
                keep_alive,
            } => {
                let res = self.connect_peer(peer_id, addr.clone());
                if res.is_ok() {
                    let endpoint = ConnectedPoint::Dialer { address: addr };
                    self.connection_manager.insert(peer_id, endpoint, keep_alive.clone());
                    self.connection_manager.set_keep_alive(&peer_id, keep_alive);
                }
                Self::send_response(CommunicationResults::EstablishConnectionResult(res), sender);
            }
            CommunicationRequest::CloseConnection(peer_id) => {
                self.connection_manager.remove_connection(&peer_id);
                Self::send_response(CommunicationResults::CloseConnectionAck, sender);
            }
            CommunicationRequest::CheckConnection(peer_id) => {
                let is_connected = Swarm::is_connected(&self.swarm, &peer_id);
                let res = CommunicationResults::CheckConnectionResult { peer_id, is_connected };
                Self::send_response(res, sender);
            }
            CommunicationRequest::GetSwarmInfo => {
                let peer_id = *Swarm::local_peer_id(&self.swarm);
                let listeners = Swarm::listeners(&self.swarm).cloned().collect();
                let connections = self.connection_manager.current_connections();
                let res = CommunicationResults::SwarmInfo {
                    peer_id,
                    listeners,
                    connections,
                };
                Self::send_response(res, sender);
            }
            CommunicationRequest::StartListening(addr) => {
                let res = self.start_listening(addr);
                Self::send_response(CommunicationResults::StartListeningResult(res), sender);
            }
            CommunicationRequest::RemoveListener => {
                let result = if let Some(listener_id) = self.listener.take() {
                    Swarm::remove_listener(&mut self.swarm, listener_id)
                } else {
                    Err(())
                };
                let res = CommunicationResults::RemoveListenerResult(result);
                Self::send_response(res, sender);
            }
            CommunicationRequest::BanPeer(peer_id) => {
                Swarm::ban_peer_id(&mut self.swarm, peer_id);
                let res = CommunicationResults::BannedPeerAck(peer_id);
                Self::send_response(res, sender);
            }
            CommunicationRequest::UnbanPeer(peer_id) => {
                Swarm::unban_peer_id(&mut self.swarm, peer_id);
                let res = CommunicationResults::UnbannedPeerAck(peer_id);
                Self::send_response(res, sender);
            }
            CommunicationRequest::SetRelay(config) => {
                let res = self.set_relay(config);
                Self::send_response(CommunicationResults::SetRelayResult(res), sender);
            }
            CommunicationRequest::ConfigureFirewall(rule) => {
                self.configure_firewall(rule);
                Self::send_response(CommunicationResults::ConfigureFirewallAck, sender);
            }
            CommunicationRequest::Shutdown => unreachable!(),
        }
    }

    // Handle incoming enveloped from either a peer directly or via the relay peer.
    fn handle_incoming_envelope(&mut self, peer_id: PeerId, request_id: RequestId, request: RequestEnvelope<Req>) {
        if Swarm::local_peer_id(&self.swarm).to_string() != request.target {
            return;
        }
        if let Ok(source) = PeerId::from_str(&request.source) {
            let is_active_direct = peer_id == source && self.connection_manager.is_active_connection(&peer_id);
            let from_relay = match self.relay {
                RelayConfig::RelayAlways {
                    peer_id: relay_id,
                    addr: _,
                }
                | RelayConfig::RelayBackup {
                    peer_id: relay_id,
                    addr: _,
                } => peer_id == relay_id,
                RelayConfig::NoRelay => false,
            };
            let is_permitted = self
                .firewall
                .is_permitted(request.message.clone(), source, RequestDirection::In);

            if (is_active_direct || from_relay) && is_permitted {
                if let Some(res) = self.ask_client(request.message) {
                    let _ = self.swarm.send_response(request_id, res);
                }
            }
        }
    }

    // Send incoming request to the client.
    // Eventually other swarm events lik e.g. incoming connection should also be send to some top level actor.
    fn handle_swarm_event<HandleErr>(&mut self, event: SwarmEvent<P2PEvent<RequestEnvelope<Req>, Res>, HandleErr>) {
        match event {
            SwarmEvent::Behaviour(behaviour_event) => match behaviour_event {
                P2PEvent::RequestResponse(boxed_event) => {
                    if let P2PReqResEvent::Req {
                        peer_id,
                        request_id,
                        request,
                    } = boxed_event.deref().clone()
                    {
                        self.handle_incoming_envelope(peer_id, request_id, request);
                    }
                }
                P2PEvent::Identify(_) | P2PEvent::Mdns(_) => {}
            },
            SwarmEvent::ConnectionEstablished {
                peer_id,
                endpoint,
                num_established: _,
            } => {
                self.connection_manager.insert(peer_id, endpoint, KeepAlive::None);
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                endpoint: ConnectedPoint::Dialer { address },
                num_established: 0,
                cause: _,
            } => {
                // Re-establish the connection if it was configured.
                if !self.connection_manager.is_keep_alive(&peer_id) || self.connect_peer(peer_id, address).is_err() {
                    self.connection_manager.remove_connection(&peer_id);
                }
            }
            _ => {}
        }
    }
}
