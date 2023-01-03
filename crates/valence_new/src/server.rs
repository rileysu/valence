use std::error::Error;
use std::iter::FusedIterator;
use std::net::{IpAddr, SocketAddr};
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use bevy_ecs::prelude::*;
use flume::{Receiver, Sender};
use rand::rngs::OsRng;
use rsa::{PublicKeyParts, RsaPrivateKey};
use tokio::runtime::{Handle, Runtime};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::instrument;
use uuid::Uuid;
use valence_nbt::{compound, Compound, List};
use valence_protocol::{ident, Username};

use crate::biome::{Biome, BiomeId};
use crate::client::Client;
use crate::config::{AsyncCallbacks, Config, ConnectionMode};
use crate::dimension::{Dimension, DimensionId};
use crate::player_textures::SignedPlayerTextures;
use crate::server::connect::do_accept_loop;
use crate::server::packet_manager::{PlayPacketReceiver, PlayPacketSender};

mod byte_channel;
mod connect;
mod packet_manager;

/// Contains global server state accessible as a [`Resource`].
#[derive(Resource)]
pub struct Server {
    /// Incremented on every tick.
    current_tick: i64,
    last_tick_duration: Duration,
    shared: SharedServer,
}

impl Deref for Server {
    type Target = SharedServer;

    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

impl Server {
    /// Provides a reference to the [`SharedServer`].
    pub fn shared(&self) -> &SharedServer {
        &self.shared
    }

    /// Returns the number of ticks that have elapsed since the server began.
    pub fn current_tick(&self) -> i64 {
        self.current_tick
    }

    /// Returns the amount of time taken to execute the previous tick, not
    /// including the time spent sleeping.
    pub fn last_tick_duration(&mut self) -> Duration {
        self.last_tick_duration
    }
}

/// The subset of global server state which can be shared between threads.
///
/// `SharedServer`s are internally refcounted and are inexpensive to clone.
#[derive(Clone)]
pub struct SharedServer(Arc<SharedServerInner>);

struct SharedServerInner {
    address: SocketAddr,
    tick_rate: i64,
    connection_mode: ConnectionMode,
    compression_threshold: Option<u32>,
    max_connections: usize,
    incoming_capacity: usize,
    outgoing_capacity: usize,
    /// The tokio handle used by the server.
    tokio_handle: Handle,
    /// Holding a runtime handle is not enough to keep tokio working. We need
    /// to store the runtime here so we don't drop it.
    _tokio_runtime: Option<Runtime>,
    dimensions: Vec<Dimension>,
    biomes: Vec<Biome>,
    /// Contains info about dimensions, biomes, and chats.
    /// Sent to all clients when joining.
    registry_codec: Compound,
    /// The instant the server was started.
    start_instant: Instant,
    /// Sender for new clients past the login stage.
    new_clients_send: Sender<NewClientMessage>,
    /// Receiver for new clients past the login stage.
    new_clients_recv: Receiver<NewClientMessage>,
    /// A semaphore used to limit the number of simultaneous connections to the
    /// server. Closing this semaphore stops new connections.
    connection_sema: Arc<Semaphore>,
    /// The result that will be returned when the server is shut down.
    shutdown_result: Mutex<Option<anyhow::Result<()>>>,
    /// The RSA keypair used for encryption with clients.
    rsa_key: RsaPrivateKey,
    /// The public part of `rsa_key` encoded in DER, which is an ASN.1 format.
    /// This is sent to clients during the authentication process.
    public_key_der: Box<[u8]>,
    /// For session server requests.
    http_client: reqwest::Client,
}

impl SharedServer {
    /// Gets the socket address this server is bound to.
    pub fn address(&self) -> SocketAddr {
        self.0.address
    }

    /// Gets the configured tick rate of this server.
    pub fn tick_rate(&self) -> i64 {
        self.0.tick_rate
    }

    /// Gets the connection mode of the server.
    pub fn connection_mode(&self) -> &ConnectionMode {
        &self.0.connection_mode
    }

    /// Gets the compression threshold for packets. `None` indicates no
    /// compression.
    pub fn compression_threshold(&self) -> Option<u32> {
        self.0.compression_threshold
    }

    /// Gets the maximum number of connections allowed to the server at once.
    pub fn max_connections(&self) -> usize {
        self.0.max_connections
    }

    /// Gets the configured incoming capacity.
    pub fn incoming_capacity(&self) -> usize {
        self.0.incoming_capacity
    }

    /// Gets the configured outgoing incoming capacity.
    pub fn outgoing_capacity(&self) -> usize {
        self.0.outgoing_capacity
    }

    /// Gets a handle to the tokio instance this server is using.
    pub fn tokio_handle(&self) -> &Handle {
        &self.0.tokio_handle
    }

    /// Obtains a [`Dimension`] by using its corresponding [`DimensionId`].
    ///
    /// It is safe but unspecified behavior to call this function using a
    /// [`DimensionId`] not originating from the configuration used to construct
    /// the server.
    pub fn dimension(&self, id: DimensionId) -> &Dimension {
        self.0
            .dimensions
            .get(id.0 as usize)
            .expect("invalid dimension ID")
    }

    /// Returns an iterator over all added dimensions and their associated
    /// [`DimensionId`].
    pub fn dimensions(&self) -> impl FusedIterator<Item = (DimensionId, &Dimension)> + Clone {
        self.0
            .dimensions
            .iter()
            .enumerate()
            .map(|(i, d)| (DimensionId(i as u16), d))
    }

    /// Obtains a [`Biome`] by using its corresponding [`BiomeId`].
    ///
    /// It is safe but unspecified behavior to call this function using a
    /// [`BiomeId`] not originating from the configuration used to construct
    /// the server.
    pub fn biome(&self, id: BiomeId) -> &Biome {
        self.0.biomes.get(id.0 as usize).expect("invalid biome ID")
    }

    /// Returns an iterator over all added biomes and their associated
    /// [`BiomeId`] in ascending order.
    pub fn biomes(
        &self,
    ) -> impl ExactSizeIterator<Item = (BiomeId, &Biome)> + DoubleEndedIterator + FusedIterator + Clone
    {
        self.0
            .biomes
            .iter()
            .enumerate()
            .map(|(i, b)| (BiomeId(i as u16), b))
    }

    pub(crate) fn registry_codec(&self) -> &Compound {
        &self.0.registry_codec
    }

    /// Returns the instant the server was started.
    pub fn start_instant(&self) -> Instant {
        self.0.start_instant
    }

    /// Immediately stops new connections to the server and initiates server
    /// shutdown. The given result is returned through [`start_server`].
    ///
    /// You may want to disconnect all players with a message prior to calling
    /// this function.
    pub fn shutdown<E>(&self, res: Result<(), E>)
    where
        E: Into<anyhow::Error>,
    {
        self.0.connection_sema.close();
        *self.0.shutdown_result.lock().unwrap() = Some(res.map_err(|e| e.into()));
    }
}

/// Contains information about a new client joining the server.
#[non_exhaustive]
pub struct NewClientInfo {
    /// The username of the new client.
    pub username: Username<String>,
    /// The UUID of the new client.
    pub uuid: Uuid,
    /// The remote address of the new client.
    pub ip: IpAddr,
    // TODO: replace "textures" with game profile.
    /// The new client's player textures. May be `None` if the client does not
    /// have a skin or cape.
    pub textures: Option<SignedPlayerTextures>,
}

struct NewClientMessage {
    info: NewClientInfo,
    send: PlayPacketSender,
    recv: PlayPacketReceiver,
    permit: OwnedSemaphorePermit,
}

/// Consumes the configuration and starts the Minecraft server.
///
/// This function blocks the current thread and returns once the server has
/// shut down, a runtime error occurs, or the configuration is found to
/// be invalid.
pub fn run_server(
    mut cfg: Config,
    stage: impl Stage,
    callbacks: impl AsyncCallbacks,
) -> anyhow::Result<()> {
    ensure!(
        cfg.tick_rate > 0,
        "configured tick rate must be greater than zero"
    );
    ensure!(
        cfg.incoming_capacity > 0,
        "configured incoming packet capacity must be nonzero"
    );
    ensure!(
        cfg.outgoing_capacity > 0,
        "configured outgoing packet capacity must be nonzero"
    );

    let rsa_key = RsaPrivateKey::new(&mut OsRng, 1024)?;

    let public_key_der =
        rsa_der::public_key_to_der(&rsa_key.n().to_bytes_be(), &rsa_key.e().to_bytes_be())
            .into_boxed_slice();

    let runtime = if cfg.tokio_handle.is_none() {
        Some(Runtime::new()?)
    } else {
        None
    };

    let tokio_handle = match &runtime {
        Some(rt) => rt.handle().clone(),
        None => cfg.tokio_handle.unwrap(),
    };

    let registry_codec = make_registry_codec(&cfg.dimensions, &cfg.biomes);

    let (new_clients_send, new_clients_recv) = flume::bounded(64);

    let shared = SharedServer(Arc::new(SharedServerInner {
        address: cfg.address,
        tick_rate: cfg.tick_rate,
        connection_mode: cfg.connection_mode,
        compression_threshold: cfg.compression_threshold,
        max_connections: cfg.max_connections,
        incoming_capacity: cfg.incoming_capacity,
        outgoing_capacity: cfg.outgoing_capacity,
        tokio_handle,
        _tokio_runtime: runtime,
        dimensions: vec![],
        biomes: vec![],
        registry_codec,
        start_instant: Instant::now(),
        new_clients_send,
        new_clients_recv,
        connection_sema: Arc::new(Semaphore::new(cfg.max_connections)),
        shutdown_result: Mutex::new(None),
        rsa_key,
        public_key_der,
        http_client: Default::default(),
    }));

    let server = Server {
        current_tick: 0,
        last_tick_duration: Duration::default(),
        shared,
    };

    let shared = server.shared.clone();
    let _guard = shared.tokio_handle().enter();

    cfg.world.insert_resource(server);

    let mut schedule = Schedule::default();

    // TODO: add systems.
    schedule.add_stage("user stage", stage);

    let callbacks = Arc::new(callbacks);
    let mut tick_start = Instant::now();
    let full_tick_duration = Duration::from_secs_f64((shared.tick_rate() as f64).recip());

    loop {
        cfg.world.clear_trackers();

        // Stop the server if it was shut down.
        if let Some(res) = shared.0.shutdown_result.lock().unwrap().take() {
            return res;
        }

        // Spawn new client entities.
        for _ in 0..shared.0.new_clients_recv.len() {
            let Ok(msg) = shared.0.new_clients_recv.try_recv() else {
                break
            };

            cfg.world.spawn(Client::new());
        }

        // Run the scheduled stages.
        schedule.run_once(&mut cfg.world);

        let mut server = cfg.world.resource_mut::<Server>();

        // Initialize the accept loop after we run the schedule for the first time. This
        // way, lengthy initialization work can happen without any players connecting
        // before it is finished.
        if server.current_tick == 0 {
            tokio::spawn(do_accept_loop(shared.clone(), callbacks.clone()));
        }

        // Sleep until the next tick.
        server.last_tick_duration = tick_start.elapsed();
        thread::sleep(full_tick_duration.saturating_sub(server.last_tick_duration));
        server.current_tick += 1;
        tick_start = Instant::now();
    }
}

fn make_registry_codec(dimensions: &[Dimension], biomes: &[Biome]) -> Compound {
    let dimensions = dimensions
        .iter()
        .enumerate()
        .map(|(id, dim)| {
            compound! {
                "name" => DimensionId(id as u16).dimension_type_name(),
                "id" => id as i32,
                "element" => dim.to_dimension_registry_item(),
            }
        })
        .collect();

    let biomes = biomes
        .iter()
        .enumerate()
        .map(|(id, biome)| biome.to_biome_registry_item(id as i32))
        .collect();

    compound! {
        ident!("dimension_type") => compound! {
            "type" => ident!("dimension_type"),
            "value" => List::Compound(dimensions),
        },
        ident!("worldgen/biome") => compound! {
            "type" => ident!("worldgen/biome"),
            "value" => {
                List::Compound(biomes)
            }
        },
        ident!("chat_type") => compound! {
            "type" => ident!("chat_type"),
            "value" => List::Compound(vec![]),
        },
    }
}
