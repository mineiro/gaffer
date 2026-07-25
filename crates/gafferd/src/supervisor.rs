//! The reconciler: the only place that touches hardware.
//!
//! One task, one loop, one `select!`. Everything that mutates shared state does
//! so synchronously and hands the supervisor a nudge; the supervisor owns all
//! I/O and spawns it so a slow or unreachable light never stalls the loop.
//!
//! The cadences are inherited from `luz` and carried through Photon, because
//! they are tuned to the hardware rather than to any framework:
//!
//! * **90 ms** debounce — coalesces a slider drag or a key held down into one PUT.
//! * **15 s** refresh — catches changes made from the Elgato app or another host.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gaffer_core::LightState;
use tokio::sync::{RwLock, mpsc};
use tokio::time::sleep_until;
use tracing::{debug, info, warn};

use crate::dbus::{Publisher, Request};
use crate::discovery::{Discovered, Discovery, DiscoveryEvent};
use crate::elgato::{self, AccessoryInfo, PartialState};
use crate::world::{Changed, Endpoint, GroupSnapshot, LightId, LightRecord, World};

/// Coalesce rapid changes into a single PUT.
const DEBOUNCE: Duration = Duration::from_millis(90);
/// How often to re-read every light's authoritative state.
const REFRESH_INTERVAL: Duration = Duration::from_secs(15);

/// The result of a piece of hardware I/O, reported back to the loop.
#[derive(Debug)]
enum Io {
    State { id: LightId, result: Result<PartialState, String>, pushed: bool },
    Info { id: LightId, info: Box<AccessoryInfo> },
}

/// Owns discovery, hardware I/O, and the reconcile schedule.
#[derive(Debug)]
pub struct Supervisor {
    world: Arc<RwLock<World>>,
    publisher: Publisher,
    discovery: Discovery,
    client: reqwest::Client,
    requests: mpsc::Receiver<Request>,
    discoveries: mpsc::Receiver<DiscoveryEvent>,
    io_tx: mpsc::Sender<Io>,
    io_rx: mpsc::Receiver<Io>,
    next_refresh: Instant,
}

impl Supervisor {
    pub fn new(
        world: Arc<RwLock<World>>,
        publisher: Publisher,
        discovery: Discovery,
        client: reqwest::Client,
        requests: mpsc::Receiver<Request>,
        discoveries: mpsc::Receiver<DiscoveryEvent>,
    ) -> Self {
        let (io_tx, io_rx) = mpsc::channel(64);
        Self {
            world,
            publisher,
            discovery,
            client,
            requests,
            discoveries,
            io_tx,
            io_rx,
            next_refresh: Instant::now() + REFRESH_INTERVAL,
        }
    }

    /// Run until `shutdown` resolves.
    pub async fn run(mut self, shutdown: impl Future<Output = ()> + Send) {
        tokio::pin!(shutdown);
        info!("supervisor running");

        loop {
            let deadline = self.deadline().await;

            tokio::select! {
                biased;
                () = &mut shutdown => break,
                Some(request) = self.requests.recv() => self.on_request(request).await,
                Some(event) = self.discoveries.recv() => self.on_discovery(event).await,
                Some(io) = self.io_rx.recv() => self.on_io(io).await,
                () = sleep_until(deadline.into()) => self.on_deadline().await,
            }
        }

        info!("supervisor stopping");
        self.discovery.shutdown();
    }

    /// When the loop next has scheduled work: the earliest pending push, or the
    /// next refresh, whichever comes first.
    async fn deadline(&self) -> Instant {
        let world = self.world.read().await;
        world
            .iter()
            .filter_map(|light| light.dirty_since)
            .map(|since| since + DEBOUNCE)
            .fold(self.next_refresh, Instant::min)
    }

    async fn on_deadline(&mut self) {
        let now = Instant::now();
        self.flush_dirty(now).await;

        if now >= self.next_refresh {
            self.next_refresh = now + REFRESH_INTERVAL;
            self.refresh_all().await;
        }
    }

    async fn on_request(&mut self, request: Request) {
        match request {
            // The world was already mutated by the interface that took the
            // command; what is owed here is the announcement. The push itself
            // follows from `deadline()` noticing the newly dirty lights.
            Request::Applied { changes, group_before, group_after } => {
                for (id, changed) in changes {
                    self.publisher.notify_light(&id, changed).await;
                }
                self.emit_group_diff(group_before, group_after).await;
            }
            Request::RefreshAll => self.refresh_all().await,
            Request::Rescan => {
                let fullnames: Vec<String> =
                    self.world.read().await.iter().map(|light| light.fullname.clone()).collect();
                self.discovery.verify(&fullnames);
                self.refresh_all().await;
            }
            Request::Identify(ids) => {
                for id in ids {
                    let Some(url) = self.base_url(&id).await else {
                        continue;
                    };
                    let client = self.client.clone();
                    tokio::spawn(async move {
                        if let Err(error) = elgato::identify(&client, &url).await {
                            debug!(%id, error = %format!("{error:#}"), "identify failed");
                        }
                    });
                }
            }
        }
    }

    async fn on_discovery(&mut self, event: DiscoveryEvent) {
        match event {
            DiscoveryEvent::Found(found) => self.on_found(*found).await,
            DiscoveryEvent::Lost(id) => self.on_lost(&id).await,
        }
    }

    async fn on_found(&mut self, found: Discovered) {
        let endpoint = Endpoint { host: found.host, addr: found.addr, port: found.port };

        let is_new = {
            let world = self.world.read().await;
            world.get(&found.id).is_none()
        };

        if is_new {
            let record = LightRecord {
                id: found.id.clone(),
                fullname: found.fullname,
                name: found.name,
                model: found.model,
                firmware: String::new(),
                endpoint,
                desired: LightState::default(),
                reported: None,
                last_error: String::new(),
                dirty_since: None,
            };

            let before = {
                let mut world = self.world.write().await;
                let before = GroupSnapshot::of(&world);
                world.insert(record);
                before
            };

            if let Err(error) = self.publisher.add_light(&found.id).await {
                warn!(id = %found.id, %error, "could not publish light on the bus");
            }
            self.publish_group_change(before).await;
            self.refresh_one(&found.id).await;
        } else {
            // A re-resolve: the address may have moved. Keep state, update route.
            self.update(&found.id, |light| {
                light.endpoint = endpoint;
                light.fullname = found.fullname;
                if !found.model.is_empty() {
                    light.model = found.model;
                }
            })
            .await;
            self.refresh_one(&found.id).await;
        }
    }

    async fn on_lost(&mut self, id: &LightId) {
        let before = {
            let mut world = self.world.write().await;
            let before = GroupSnapshot::of(&world);
            if world.remove(id).is_none() {
                return;
            }
            before
        };

        if let Err(error) = self.publisher.remove_light(id).await {
            debug!(%id, %error, "could not withdraw light from the bus");
        }
        self.publish_group_change(before).await;
    }

    async fn on_io(&mut self, io: Io) {
        match io {
            Io::State { id, result, pushed } => match result {
                Ok(partial) => {
                    self.update(&id, |light| {
                        let mut merged = partial.merge_into(light.desired);
                        // Keep the requested Kelvin when the echo lands on the
                        // same mired: the two are one hardware setting, and
                        // showing 4202K after the user typed 4200k is an
                        // artifact of reciprocal rounding, not information.
                        if gaffer_core::color::same_mired(merged.kelvin, light.desired.kelvin) {
                            merged.kelvin = light.desired.kelvin;
                        }
                        light.reported = Some(merged);
                        light.last_error.clear();
                        // Adopt the hardware's truth only when no push is owed.
                        // Mid-flight, `desired` is the more recent intent and
                        // overwriting it would undo the user's command.
                        if light.dirty_since.is_none() {
                            light.desired = merged;
                        }
                    })
                    .await;
                    if pushed {
                        debug!(%id, "push acknowledged");
                    }
                }
                Err(error) => {
                    self.update(&id, |light| {
                        light.reported = None; // no report *is* offline
                        light.last_error = error.clone();
                    })
                    .await;
                    debug!(%id, %error, "light unreachable");
                }
            },
            Io::Info { id, info } => {
                self.update(&id, |light| {
                    if !info.display_name.is_empty() {
                        light.name = info.display_name.clone();
                    }
                    if !info.product_name.is_empty() {
                        light.model = info.product_name.clone();
                    }
                    light.firmware = info.firmware_version.clone();
                })
                .await;
            }
        }
    }

    /// Push every light whose debounce window has elapsed.
    async fn flush_dirty(&self, now: Instant) {
        let due: Vec<(LightId, String, LightState)> = {
            let mut world = self.world.write().await;
            let ids: Vec<LightId> = world
                .iter()
                .filter(|light| {
                    light
                        .dirty_since
                        .is_some_and(|since| now.saturating_duration_since(since) >= DEBOUNCE)
                })
                .map(|light| light.id.clone())
                .collect();

            ids.into_iter()
                .filter_map(|id| {
                    let light = world.get_mut(&id)?;
                    // Cleared *before* the request goes out, so a command
                    // arriving mid-flight marks the light dirty again and gets
                    // its own push rather than being swallowed.
                    light.dirty_since = None;
                    Some((id, light.endpoint.base_url(), light.desired))
                })
                .collect()
        };

        for (id, url, state) in due {
            let client = self.client.clone();
            let io = self.io_tx.clone();
            tokio::spawn(async move {
                let result =
                    elgato::put_state(&client, &url, state).await.map_err(|e| format!("{e:#}"));
                let _ = io.send(Io::State { id, result, pushed: true }).await;
            });
        }
    }

    async fn refresh_all(&self) {
        for id in self.world.read().await.ids() {
            self.refresh_one(&id).await;
        }
    }

    async fn refresh_one(&self, id: &LightId) {
        let (url, needs_info) = {
            let world = self.world.read().await;
            let Some(light) = world.get(id) else { return };
            (light.endpoint.base_url(), light.firmware.is_empty())
        };

        let client = self.client.clone();
        let io = self.io_tx.clone();
        let light_id = id.clone();
        let state_url = url.clone();
        tokio::spawn(async move {
            let result = elgato::get_state(&client, &state_url).await.map_err(|e| format!("{e:#}"));
            let _ = io.send(Io::State { id: light_id, result, pushed: false }).await;
        });

        if needs_info {
            let client = self.client.clone();
            let io = self.io_tx.clone();
            let light_id = id.clone();
            tokio::spawn(async move {
                // Best-effort: a light that answers /lights but not
                // /accessory-info is still perfectly usable.
                if let Ok(info) = elgato::get_info(&client, &url).await {
                    let _ = io.send(Io::Info { id: light_id, info: Box::new(info) }).await;
                }
            });
        }
    }

    async fn base_url(&self, id: &LightId) -> Option<String> {
        self.world.read().await.get(id).map(|light| light.endpoint.base_url())
    }

    /// Edit one light, then emit exactly the property changes that resulted.
    async fn update(&self, id: &LightId, edit: impl FnOnce(&mut LightRecord)) {
        let (changed, before, after) = {
            let mut world = self.world.write().await;
            let before = GroupSnapshot::of(&world);

            let Some(light) = world.get_mut(id) else {
                return;
            };
            let previous = light.clone();
            edit(light);
            let changed = Changed::between(&previous, light);

            let after = GroupSnapshot::of(&world);
            (changed, before, after)
        }; // the write guard is dropped here, before any object-server call

        self.publisher.notify_light(id, changed).await;
        self.emit_group_diff(before, after).await;
    }

    /// Emit group changes relative to a snapshot taken before a membership edit.
    async fn publish_group_change(&self, before: GroupSnapshot) {
        let after = GroupSnapshot::of(&*self.world.read().await);
        self.emit_group_diff(before, after).await;
    }

    async fn emit_group_diff(&self, before: GroupSnapshot, after: GroupSnapshot) {
        self.publisher.notify_group(GroupSnapshot::diff(before, after)).await;
        if GroupSnapshot::membership_changed(before, after) {
            self.publisher.notify_manager().await;
        }
    }
}
