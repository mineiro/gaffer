//! gafferd — owns light discovery and state, and speaks D-Bus.
//!
//! The daemon exists so light state outlives any window. Discovery, the
//! reconcile loop, and the protocol live here once; every UI — the `gaffer` CLI,
//! a panel module, a future gpui app — is a thin client of the session bus.
//!
//! Started on demand via D-Bus activation, which defers to systemd so the
//! process gets a proper cgroup, journal and restart policy. See `data/`.

mod config;
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

/// Handle the arguments a daemon is nonetheless expected to answer.
///
/// Hand-rolled rather than pulling clap into the daemon: gafferd takes no
/// options, and the only thing that matters is not silently booting a daemon
/// when someone types `--help` and waits for output that never comes. An
/// unknown argument is an error for the same reason — a typo in a unit file
/// should fail loudly rather than start a daemon that ignores it.
///
/// Returns `true` if the caller should exit immediately.
fn handled_trivial_args() -> bool {
    // Only the first argument is inspected: gafferd accepts no options, so
    // anything at all beyond the program name is either a query or a mistake.
    let Some(arg) = std::env::args().nth(1) else {
        return false;
    };

    match arg.as_str() {
        "--version" | "-V" => {
            println!("gafferd {} ({})", env!("CARGO_PKG_VERSION"), env!("GAFFER_BUILD_ID"));
        }
        "--help" | "-h" => println!(
            "gafferd {}\n\n\
             The gaffer daemon. Owns Elgato Key Light discovery and state, and\n\
             serves them on the D-Bus session bus as {}.\n\n\
             Takes no options. Started on demand by D-Bus activation; run\n\
             `gaffer` to control lights, and set GAFFER_LOG=debug for detail.",
            env!("CARGO_PKG_VERSION"),
            dbus::BUS_NAME
        ),
        other => {
            eprintln!("gafferd: unexpected argument `{other}`; try --help");
            std::process::exit(2);
        }
    }

    true
}

#[tokio::main]
async fn main() -> Result<()> {
    if handled_trivial_args() {
        return Ok(());
    }

    init_tracing();
    info!(version = env!("CARGO_PKG_VERSION"), "gafferd starting");

    // Gangs and scenes are user intent and outlive a restart; everything else
    // about a light is re-discovered.
    let mut initial = World::default();
    if let Some(path) = config::Config::path() {
        let config = config::Config::load(&path);
        let (links, scenes) = (config.links(), config.scenes());
        if !links.is_empty() || !scenes.is_empty() {
            info!(
                gangs = links.len(),
                scenes = scenes.len(),
                path = %path.display(),
                "restored saved state"
            );
        }
        initial.set_links(links);
        initial.set_scenes(scenes);
    }
    let world = Arc::new(RwLock::new(initial));
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
