// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

// Compilation
#![allow(clippy::module_inception)]
#![warn(unused_extern_crates)]
#![forbid(unsafe_code)]
// Documentation
#![cfg_attr(nightly, feature(doc_cfg, external_doc))]
#![cfg_attr(nightly, doc(include = "../documentation/concepts/network_server.md"))]

#[macro_use]
extern crate derivative;
#[macro_use]
extern crate tracing;
#[macro_use]
extern crate snarkos_metrics;

pub mod consensus;
pub use consensus::*;

pub mod environment;
pub use environment::*;

pub mod errors;
pub use errors::*;

pub mod inbound;
pub use inbound::*;

pub mod message;
pub use message::*;

pub mod outbound;
pub use outbound::*;

pub mod peers;
pub use peers::*;

use crate::ConnWriter;
use snarkvm_objects::Storage;

use parking_lot::{Mutex, RwLock};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{task, time::sleep};

pub const HANDSHAKE_PATTERN: &str = "Noise_XXpsk3_25519_ChaChaPoly_SHA256";
pub const HANDSHAKE_PSK: &[u8] = b"b765e427e836e0029a1e2a22ba60c52a"; // the PSK must be 32B
pub const MAX_MESSAGE_SIZE: usize = 8 * 1024 * 1024; // 8MiB
pub const NOISE_BUF_LEN: usize = 65535;
pub const NOISE_TAG_LEN: usize = 16;
/// The maximum number of block hashes that can be requested or provided in a single batch.
pub const MAX_BLOCK_SYNC_COUNT: u32 = 250;
/// The maximum number of peers shared at once in response to a `GetPeers` message.
pub const SHARED_PEER_COUNT: usize = 25;

pub(crate) type Sender = tokio::sync::mpsc::Sender<Message>;

pub(crate) type Receiver = tokio::sync::mpsc::Receiver<Message>;

/// A core data structure for operating the networking stack of this node.
// TODO: remove inner Arcs once the Node itself is passed around in an Arc or contains an inner object wrapped in an Arc (causing all the Node's contents that are not to be "cloned around" to be Arced too).
#[derive(Derivative)]
#[derivative(Clone(bound = ""))]
pub struct Node<S: Storage> {
    /// The parameters and settings of this node.
    pub environment: Environment,
    /// The inbound handler of this node.
    inbound: Arc<Inbound>,
    /// The outbound handler of this node.
    outbound: Arc<Outbound>,
    /// The list of connected and disconnected peers of this node.
    pub peer_book: Arc<RwLock<PeerBook>>,
    /// The objects related to consensus.
    pub consensus: Option<Arc<Consensus<S>>>,
    /// The tasks spawned by the node.
    tasks: Arc<Mutex<Vec<task::JoinHandle<()>>>>,
}

impl<S: Storage + Send + Sync + 'static> Node<S> {
    /// Creates a new instance of `Node`.
    pub async fn new(environment: Environment) -> Result<Self, NetworkError> {
        let channels: Arc<RwLock<HashMap<SocketAddr, Arc<ConnWriter>>>> = Default::default();
        // Create the inbound and outbound handlers.
        let inbound = Arc::new(Inbound::new(channels.clone()));
        let outbound = Arc::new(Outbound::new(channels));

        Ok(Self {
            environment,
            inbound,
            outbound,
            peer_book: Default::default(),
            consensus: None,
            tasks: Default::default(),
        })
    }

    pub fn set_consensus(&mut self, consensus: Consensus<S>) {
        self.consensus = Some(Arc::new(consensus));
    }

    /// Returns a reference to the consensus objects.
    #[inline]
    pub fn consensus(&self) -> Option<&Arc<Consensus<S>>> {
        self.consensus.as_ref()
    }

    /// Returns a reference to the consensus objects, expecting them to be available.
    #[inline]
    pub fn expect_consensus(&self) -> &Consensus<S> {
        self.consensus.as_ref().expect("no consensus!")
    }

    #[inline]
    #[doc(hidden)]
    pub fn has_consensus(&self) -> bool {
        self.consensus.is_some()
    }

    pub async fn start_services(&self) {
        let self_clone = self.clone();
        let mut receiver = self.inbound.take_receiver();
        let incoming_task = task::spawn(async move {
            loop {
                if let Err(e) = self_clone.process_incoming_messages(&mut receiver).await {
                    error!("Node error: {}", e);
                }
            }
        });
        self.register_task(incoming_task);

        let self_clone = self.clone();
        let peer_sync_interval = self.environment.peer_sync_interval();
        let peering_task = task::spawn(async move {
            loop {
                sleep(peer_sync_interval).await;
                info!("Updating peers");

                if let Err(e) = self_clone.update_peers().await {
                    error!("Peer update error: {}", e);
                }
            }
        });
        self.register_task(peering_task);

        if !self.environment.is_bootnode() {
            if let Some(ref consensus) = self.consensus() {
                let self_clone = self.clone();
                let consensus = Arc::clone(consensus);
                let transaction_sync_interval = consensus.transaction_sync_interval();
                let tx_sync_task = task::spawn(async move {
                    loop {
                        sleep(transaction_sync_interval).await;

                        if !consensus.is_syncing_blocks() {
                            info!("Updating transactions");

                            // select last seen node as block sync node
                            let sync_node = self_clone.peer_book.read().last_seen();
                            consensus.update_transactions(sync_node).await;
                        }
                    }
                });
                self.register_task(tx_sync_task);
            }
        }
    }

    pub fn shut_down(&self) {
        debug!("Shutting down");

        for handle in self.tasks.lock().drain(..).rev() {
            handle.abort();
        }
    }

    pub fn register_task(&self, handle: task::JoinHandle<()>) {
        self.tasks.lock().push(handle);
    }

    #[inline]
    pub fn local_address(&self) -> Option<SocketAddr> {
        self.environment.local_address()
    }
}
