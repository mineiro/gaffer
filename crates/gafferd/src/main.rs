//! gafferd — owns light discovery and state, and speaks D-Bus.
//!
//! The daemon exists so light state outlives any window. Discovery, the
//! reconcile loop, and the protocol live here once; every UI — the `gaffer` CLI,
//! a panel module, a future gpui app — is a thin client of the session bus.
//!
//! Started on demand via D-Bus activation, which defers to systemd so the
//! process gets a proper cgroup, journal and restart policy. See `data/`.

mod dbus;
mod discovery;
mod elgato;
mod supervisor;
mod world;

use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{RwLock, mpsc};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::dbus::Publisher;
use crate::discovery::Discovery;
use crate::supervisor::Supervisor;
use crate::world::World;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    info!(version = env!("CARGO_PKG_VERSION"), "gafferd starting");

    let world = Arc::new(RwLock::new(World::default()));
    let (requests_tx, requests_rx) = mpsc::channel(64);
    let (discoveries_tx, discoveries_rx) = mpsc::channel(64);

    // Claim the bus name only after every object is registered, so a client
    // that activates us never sees the name without the objects behind it.
    let publisher = Publisher::connect(world.clone(), requests_tx.clone()).await.context(
        "claiming io.mineiro.gaffer on the session bus (is another gafferd already running?)",
    )?;
    info!(name = dbus::BUS_NAME, "claimed bus name");

    let discovery = Discovery::start(discoveries_tx).context("starting mDNS discovery")?;
    let client = elgato::client()?;
    let connection = publisher.connection().clone();

    let supervisor =
        Supervisor::new(world, publisher, discovery, client, requests_rx, discoveries_rx);

    // Stop on a signal *or* on losing the bus name. A daemon that no longer
    // owns the name cannot be reached by any client, but would happily keep
    // pushing state to the hardware — which is worse than not running at all.
    supervisor
        .run(async move {
            tokio::select! {
                () = shutdown_signal() => {}
                () = name_lost(&connection) => info!("bus name taken over; exiting"),
            }
        })
        .await;

    Ok(())
}

/// Resolves if this connection loses the gaffer bus name.
async fn name_lost(connection: &zbus::Connection) {
    let stream = async {
        let dbus = zbus::fdo::DBusProxy::new(connection).await.ok()?;
        dbus.receive_name_lost().await.ok()
    };
    let Some(mut lost) = stream.await else {
        return std::future::pending().await;
    };

    while let Some(signal) = lost.next().await {
        if signal.args().is_ok_and(|args| args.name.as_str() == dbus::BUS_NAME) {
            return;
        }
    }
    std::future::pending().await
}

/// Resolves on SIGINT or SIGTERM. systemd sends the latter on `stop`.
async fn shutdown_signal() {
    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(stream) => stream,
        Err(_) => return std::future::pending().await,
    };
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(stream) => stream,
        Err(_) => return std::future::pending().await,
    };

    tokio::select! {
        _ = interrupt.recv() => info!("interrupted"),
        _ = terminate.recv() => info!("terminated"),
    }
}

/// Log to stderr; systemd routes that to the journal for user units.
fn init_tracing() {
    let filter = EnvFilter::try_from_env("GAFFER_LOG")
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("the fallback filter is valid");

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
