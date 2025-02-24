// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under the MIT license <LICENSE-MIT
// http://opensource.org/licenses/MIT> or the Modified BSD license <LICENSE-BSD
// https://opensource.org/licenses/BSD-3-Clause>, at your option. This file may not be copied,
// modified, or distributed except according to those terms. Please review the Licences for the
// specific language governing permissions and limitations relating to use of the SAFE Network
// Software.

//! This example connects to bootstrap node, waits for other client node contacts and starts
//! communicating with them.
//!
//! Usage:
//! ```
//! $ RUST_LOG=client_node=info cargo run --example client_node -- -b '[{"peer_addr":
//! "127.0.0.1:5000","peer_cert_der":[48,130,..]}]'
//! ```

#[macro_use]
extern crate log;
#[macro_use]
extern crate unwrap;

mod common;

use bytes::Bytes;
use common::Rpc;
use crc::crc32;
use crossbeam_channel as mpmc;
use env_logger;
use quic_p2p::{Builder, Config, Event, NodeInfo, Peer, QuicP2p};
use rand::{self, seq::IteratorRandom, RngCore};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use structopt::StructOpt;

/// Client node will be connecting to bootstrap node from which it will receive contacts
/// of other client nodes. Then this node will connect with all other client nodes and
/// try to exchange some data with them.
#[derive(Debug, StructOpt)]
struct CliArgs {
    #[structopt(flatten)]
    quic_p2p_opts: Config,
}

struct ClientNode {
    qp2p: QuicP2p,
    bootstrap_node_info: NodeInfo,
    /// It's optional just to fight the borrow checker.
    event_rx: Option<mpmc::Receiver<Event>>,
    /// Other nodes we will be communicating with.
    client_nodes: HashSet<NodeInfo>,
    our_ci: NodeInfo,
    sent_messages: usize,
    received_messages: usize,
    /// Large message to send
    large_msg: Bytes,
    /// Smaller message to send
    small_msg: Bytes,
    peer_states: HashMap<SocketAddr, bool>,
}

const LARGE_MSG_SIZE: usize = 20 * 1024 * 1024; // 20 MB
const SMALL_MSG_SIZE: usize = 16 * 1024; // 16 KB

fn main() {
    env_logger::init();

    let config = CliArgs::from_args();
    println!("{:?}", config);

    match ClientNode::new(config.quic_p2p_opts) {
        Ok(mut c) => c.run(),
        Err(e) => eprintln!("{}", e),
    }
}

impl ClientNode {
    fn new(config: Config) -> Result<Self, String> {
        // Choose a random bootstrap node.
        let bootstrap_node_info = config
            .hard_coded_contacts
            .iter()
            .choose(&mut rand::thread_rng())
            .ok_or_else(|| "No valid bootstrap node was provided.".to_string())?
            .clone();

        let (event_tx, event_rx) = mpmc::unbounded();
        let mut qp2p = unwrap!(Builder::new(event_tx).with_config(config).build());

        let large_msg = Bytes::from(random_data_with_hash(LARGE_MSG_SIZE));
        assert!(hash_correct(&large_msg));

        let small_msg = Bytes::from(random_data_with_hash(SMALL_MSG_SIZE));
        assert!(hash_correct(&small_msg));

        let our_ci = unwrap!(qp2p.our_connection_info());

        Ok(Self {
            qp2p,
            bootstrap_node_info,
            large_msg,
            small_msg,
            event_rx: Some(event_rx),
            client_nodes: Default::default(),
            our_ci,
            peer_states: Default::default(),
            sent_messages: 0,
            received_messages: 0,
        })
    }

    /// Blocks and reacts to qp2p events.
    fn run(&mut self) {
        info!("Peer started");

        // this dummy send will trigger connection
        let bootstrap_node = Peer::Node {
            node_info: self.bootstrap_node_info.clone(),
        };
        self.qp2p.send(bootstrap_node, Bytes::from(vec![1, 2, 3]));

        self.poll_qp2p_events();
    }

    fn poll_qp2p_events(&mut self) {
        let event_rx = unwrap!(self.event_rx.take());
        for event in event_rx.iter() {
            match event {
                Event::ConnectedTo { peer } => self.on_connect(peer),
                Event::NewMessage { peer_addr, msg } => self.on_msg_receive(peer_addr, msg),
                event => warn!("Unexpected event: {:?}", event),
            }
        }
    }

    fn on_connect(&mut self, peer: Peer) {
        let peer_info = match &peer {
            Peer::Node { node_info } => node_info.clone(),
            Peer::Client { .. } => panic!("In this example only Node peers are expected"),
        };
        info!("Connected with: {}", peer_info.peer_addr);

        if peer_info == self.bootstrap_node_info {
            info!("Connected to bootstrap node. Waiting for other node contacts...");
        } else if self.client_nodes.contains(&peer_info) {
            self.qp2p.send(peer.clone(), self.large_msg.clone());
            self.qp2p.send(peer, self.small_msg.clone());
            self.sent_messages += 1;
        }
    }

    fn on_msg_receive(&mut self, peer_addr: SocketAddr, msg: Bytes) {
        if self.response_from_bootstrap_node(&peer_addr) {
            let msg: Rpc = unwrap!(bincode::deserialize(&msg));
            match msg {
                Rpc::StartTest(peers) => self.connect_to_peers(peers),
            }
        } else {
            let small_msg_rcvd = self.peer_states.entry(peer_addr).or_insert(false);

            debug!("[{}] Message received: {}", peer_addr, msg.len());
            assert!(hash_correct(&msg));

            let payload_size = msg.len() - 4; // without the hash

            if payload_size == LARGE_MSG_SIZE {
                if !*small_msg_rcvd {
                    error!("[{}] Large message received before small", peer_addr);
                } else {
                    self.received_messages += 1;
                    debug!(
                        "Recv: {}/{}, Sent: {}/{}",
                        self.received_messages,
                        self.client_nodes.len(),
                        self.sent_messages,
                        self.client_nodes.len()
                    );

                    if self.received_messages == self.client_nodes.len()
                        && self.sent_messages == self.client_nodes.len()
                    {
                        info!("Done. All checks passed");
                    }
                }
            } else if payload_size == SMALL_MSG_SIZE {
                if *small_msg_rcvd {
                    error!("[{}] Small message received twice", peer_addr);
                }
                *small_msg_rcvd = true;
            }
        }
    }

    fn connect_to_peers(&mut self, peers: Vec<Peer>) {
        for peer in peers {
            let conn_info = match &peer {
                Peer::Node { node_info } => node_info.clone(),
                Peer::Client { .. } => panic!("In this example only Node peers are expected"),
            };
            if conn_info != self.our_ci {
                self.qp2p.connect_to(conn_info.clone());
                self.client_nodes.insert(conn_info);
            }
        }
    }

    fn response_from_bootstrap_node(&self, peer_addr: &SocketAddr) -> bool {
        peer_addr == &self.bootstrap_node_info.peer_addr
    }
}

fn random_data_with_hash(size: usize) -> Vec<u8> {
    let mut data = random_vec(size + 4);
    let hash = crc32::checksum_ieee(&data[4..]);
    // write hash in big endian
    data[0] = (hash >> 24) as u8;
    data[1] = ((hash >> 16) & 0xff) as u8;
    data[2] = ((hash >> 8) & 0xff) as u8;
    data[3] = (hash & 0xff) as u8;
    data
}

fn hash_correct(data: &[u8]) -> bool {
    let encoded_hash = (u32::from(data[0]) << 24)
        | (u32::from(data[1]) << 16)
        | (u32::from(data[2]) << 8)
        | u32::from(data[3]);
    let actual_hash = crc32::checksum_ieee(&data[4..]);
    encoded_hash == actual_hash
}

#[allow(unsafe_code)]
fn random_vec(size: usize) -> Vec<u8> {
    let mut ret = Vec::with_capacity(size);
    unsafe { ret.set_len(size) };
    rand::thread_rng().fill_bytes(&mut ret[..]);
    ret
}
