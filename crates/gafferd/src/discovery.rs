//! Finding lights on the network.
//!
//! Both earlier implementations parsed the DNS wire format by hand, and both
//! missed the same things: TTL expiry and goodbye packets, the cache-flush bit,
//! IPv6, and interface hotplug. `mdns-sd` handles all four, so this module is
//! mostly a translation layer: mDNS records in, [`DiscoveryEvent`]s out.
//!
//! The one piece of domain knowledge that stays here: lights are keyed on the
//! TXT `id=` field, which is the hardware MAC, **not** the mDNS instance name.
//! Renaming a light in Elgato's app changes the instance name, and keying on it
//! would make the same physical light appear as a second device.
//!
//! **An mDNS record lapsing is not a light leaving.** Discovery *finds* lights;
//! whether one is reachable is answered over HTTP, where `reported == None` is
//! the definition of offline. Treating expiry as removal dropped both lamps on
//! a desk whose records lapsed while the lamps were answering HTTP the whole
//! time, and with them gone nothing ever queried for them again. So a lapse is
//! reported as a hint to probe, and starts an active re-query that runs until
//! the record comes back.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use mdns_sd::{Receiver, ScopedIp, ServiceDaemon, ServiceEvent};
use tokio::sync::{Notify, mpsc};
use tokio::time::{Instant, sleep_until};
use tracing::{debug, info, warn};

/// The mDNS service type Elgato Key Lights advertise.
pub const ELGATO_SERVICE: &str = "_elg._tcp.local.";

/// Fallback port if a light advertises an SRV record without one.
pub const DEFAULT_PORT: u16 = 9123;

/// How soon after a record lapses to re-query. Short, but not zero, so that
/// every lamp on a desk lapsing in the same instant costs one query, not one
/// each.
const REQUERY_PROMPTLY: Duration = Duration::from_secs(1);

/// How often to re-query while any known light's record is still missing.
///
/// Each re-query restarts mdns-sd's browse, whose own back-off then sends a
/// handful of PTR queries over the following minute; this spaces those bursts.
/// A healthy desk pays nothing, because nothing is missing.
const REQUERY_INTERVAL: Duration = Duration::from_secs(90);

/// A light seen on the network.
///
/// Every string here has already been canonicalised or sanitised: an mDNS
/// record is attacker-controlled, and this is the boundary where that stops
/// being true.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    /// Canonical hardware id, from the TXT `id=` field.
    pub id: String,
    /// mDNS instance fullname, which removal events identify a record by.
    pub fullname: String,
    /// Human-readable name derived from the instance name. Superseded by the
    /// device's own `displayName` once accessory-info has been fetched.
    pub name: String,
    /// Model from the TXT `md=` field.
    pub model: String,
    pub addr: IpAddr,
    pub port: u16,
}

/// Something changed on the network.
///
/// There is deliberately no "gone": see the module note.
#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    Found(Box<Discovered>),
    /// A light's record lapsed, by goodbye packet or TTL expiry. Probe it.
    Lapsed(String),
}

/// A running mDNS browse.
pub struct Discovery {
    daemon: ServiceDaemon,
    requery: Arc<Notify>,
}

// `ServiceDaemon` is a handle to a background thread and is not `Debug`, but
// `Supervisor` derives it, so stand in with something useful.
impl std::fmt::Debug for Discovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Discovery").field("service", &ELGATO_SERVICE).finish_non_exhaustive()
    }
}

impl Discovery {
    /// Start browsing, forwarding events to `events`.
    pub fn start(events: mpsc::Sender<DiscoveryEvent>) -> Result<Self> {
        let daemon = ServiceDaemon::new().context("starting the mDNS responder")?;

        // Re-check interfaces fairly often so plugging in a dock or joining a
        // different network is picked up without a restart.
        if let Err(error) = daemon.set_ip_check_interval(15) {
            warn!(%error, "could not set the interface re-check interval");
        }

        let receiver =
            daemon.browse(ELGATO_SERVICE).context("browsing for _elg._tcp.local services")?;
        let requery = Arc::new(Notify::new());

        tokio::spawn(browse(daemon.clone(), receiver, events, requery.clone()));

        Ok(Self { daemon, requery })
    }

    /// Send a fresh PTR query for the service now.
    ///
    /// Answers arrive through the ordinary event stream, so a newly found light
    /// appears as `InterfacesAdded` a moment after this returns.
    pub fn requery(&self) {
        self.requery.notify_one();
    }

    /// Stop the responder thread.
    pub fn shutdown(&self) {
        if let Err(error) = self.daemon.shutdown() {
            warn!(%error, "mDNS responder did not shut down cleanly");
        }
    }
}

/// The browse loop: translate mDNS events, and re-query when asked to or when
/// a known light's record is missing.
async fn browse(
    daemon: ServiceDaemon,
    mut receiver: Receiver<ServiceEvent>,
    events: mpsc::Sender<DiscoveryEvent>,
    requery: Arc<Notify>,
) {
    let mut tracker = Tracker::default();

    loop {
        let due = tracker.next_requery;
        let event = tokio::select! {
            event = receiver.recv_async() => match event {
                Ok(event) => event,
                Err(_) => break, // the responder thread is gone
            },
            () = requery.notified() => {
                rebrowse(&daemon, &mut receiver, &mut tracker, "asked to rescan");
                continue;
            }
            () = sleep_until_some(due) => {
                rebrowse(&daemon, &mut receiver, &mut tracker, "a known light's record is missing");
                continue;
            }
        };

        let outgoing = match event {
            ServiceEvent::ServiceResolved(service) => match resolve(&service) {
                Some(found) => {
                    match tracker.resolved(&found.fullname, &found.id) {
                        Sighting::New => {
                            info!(id = %found.id, name = %found.name, addr = ?found.addr, "light discovered");
                        }
                        Sighting::Returned => {
                            info!(id = %found.id, addr = ?found.addr, "mDNS record is back");
                        }
                        // mDNS re-resolves the same instance repeatedly, so a
                        // perfectly stable network would otherwise fill the
                        // journal with duplicate lines.
                        Sighting::Again => {
                            debug!(id = %found.id, addr = ?found.addr, "light re-resolved")
                        }
                    }
                    Some(DiscoveryEvent::Found(Box::new(found)))
                }
                None => {
                    debug!(fullname = %service.fullname, "ignoring unusable service record");
                    None
                }
            },
            ServiceEvent::ServiceRemoved(_, fullname) => {
                tracker.removed(&fullname, Instant::now()).map(|id| {
                    info!(%id, %fullname, "mDNS record lapsed; probing the light and re-querying");
                    DiscoveryEvent::Lapsed(id)
                })
            }
            other => {
                debug!(?other, "mdns event");
                None
            }
        };

        if let Some(event) = outgoing
            && events.send(event).await.is_err()
        {
            break; // the supervisor is gone; so are we
        }
    }
    debug!("discovery stream ended");
}

/// Restart the browse, which sends a PTR query at once.
///
/// mdns-sd has no one-shot query, and `verify` only re-asks for records still
/// in its cache, which after a lapse is precisely nothing. Browsing again
/// replaces the listener; dropping the old receiver ends the old browse's
/// retransmission schedule, so the two do not stack. Records already cached
/// are replayed to the new receiver and arrive as ordinary re-resolves.
fn rebrowse(
    daemon: &ServiceDaemon,
    receiver: &mut Receiver<ServiceEvent>,
    tracker: &mut Tracker,
    why: &str,
) {
    match daemon.browse(ELGATO_SERVICE) {
        Ok(fresh) => {
            debug!(why, "re-querying {ELGATO_SERVICE}");
            *receiver = fresh;
        }
        Err(error) => warn!(%error, "could not re-query for lights"),
    }
    tracker.requeried(Instant::now());
}

async fn sleep_until_some(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// How a resolved record relates to what discovery already knew.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sighting {
    New,
    /// A light whose record had lapsed is advertising again.
    Returned,
    Again,
}

/// Which records are live, which lights have lapsed, and when to re-query.
///
/// Pure bookkeeping, separated from the event loop so the re-query policy can
/// be tested without sockets.
#[derive(Debug, Default)]
struct Tracker {
    /// Instance fullname to hardware id, for every record currently live.
    /// Removal events name only the fullname, and the rest of the daemon speaks
    /// in ids.
    live: HashMap<String, String>,
    /// Lights whose record lapsed and has not come back.
    lapsed: HashSet<String>,
    next_requery: Option<Instant>,
}

impl Tracker {
    fn resolved(&mut self, fullname: &str, id: &str) -> Sighting {
        let returned = self.lapsed.remove(id);
        if self.lapsed.is_empty() {
            self.next_requery = None;
        }
        let first = self.live.insert(fullname.to_owned(), id.to_owned()).is_none();
        match (returned, first) {
            (true, _) => Sighting::Returned,
            (false, true) if !self.live_elsewhere(fullname, id) => Sighting::New,
            _ => Sighting::Again,
        }
    }

    /// Note a lapsed record, returning the id it belonged to if that light now
    /// has no live record at all.
    fn removed(&mut self, fullname: &str, now: Instant) -> Option<String> {
        let id = self.live.remove(fullname)?;
        // A light renamed in Elgato's app advertises under a new instance name
        // while the old one ages out. The light has not lapsed; its old name has.
        if self.live_elsewhere(fullname, &id) {
            return None;
        }
        self.lapsed.insert(id.clone());
        let soon = now + REQUERY_PROMPTLY;
        self.next_requery = Some(self.next_requery.map_or(soon, |at| at.min(soon)));
        Some(id)
    }

    fn requeried(&mut self, now: Instant) {
        self.next_requery = (!self.lapsed.is_empty()).then(|| now + REQUERY_INTERVAL);
    }

    fn live_elsewhere(&self, fullname: &str, id: &str) -> bool {
        self.live.iter().any(|(name, other)| name != fullname && other == id)
    }
}

/// Turn a resolved mDNS service into a [`Discovered`], or `None` if it lacks
/// what we need to talk to it.
fn resolve(service: &mdns_sd::ResolvedService) -> Option<Discovered> {
    // Canonicalise rather than trust: devices punctuate MACs inconsistently,
    // and two spellings of one id would otherwise become two records that
    // collide on a single D-Bus object path. Also rejects an id with no usable
    // characters, which would produce the invalid path `…/lights/`.
    let id = gaffer_core::normalize_id(service.get_property_val_str("id").unwrap_or_default())?;

    // A resolved address is required; the SRV target name is never used to
    // build a URL. mDNS does not validate the characters in a received name,
    // so formatting one into a URL let a crafted advertisement inject an
    // arbitrary path — see the note on `Endpoint`. Prefer IPv4, and accept
    // IPv6 only when that is all the device offers.
    let addr = service
        .get_addresses_v4()
        .into_iter()
        .min()
        .map(IpAddr::from)
        .or_else(|| service.get_addresses().iter().map(ScopedIp::to_ip_addr).min())?;

    let port = if service.port == 0 { DEFAULT_PORT } else { service.port };

    Some(Discovered {
        id,
        fullname: service.fullname.clone(),
        // Sanitised: an instance label carries raw bytes off the wire, so this
        // is where a control character stops being able to reach a terminal.
        name: gaffer_core::sanitize(&instance_name(&service.fullname)),
        model: gaffer_core::sanitize(service.get_property_val_str("md").unwrap_or_default()),
        addr,
        port,
    })
}

/// Extract a display name from an mDNS fullname.
///
/// `Elgato\032Key\032Light\032Left._elg._tcp.local.` becomes
/// `Elgato Key Light Left`. Escapes are DNS decimal (`\032`), not C-style.
fn instance_name(fullname: &str) -> String {
    let instance = fullname.split_once("._elg._tcp").map_or(fullname, |(head, _)| head);
    unescape(instance)
}

/// Decode DNS `\DDD` decimal escapes and `\.`-style literals.
fn unescape(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut chars = label.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        // Peek at up to three digits; a decimal escape encodes one byte.
        let rest = chars.as_str();
        let digits: String = rest.chars().take(3).take_while(char::is_ascii_digit).collect();
        if digits.len() == 3
            && let Ok(byte) = digits.parse::<u8>()
        {
            out.push(char::from(byte));
            for _ in 0..3 {
                chars.next();
            }
        } else if let Some(literal) = chars.next() {
            out.push(literal); // `\.` and friends
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: &str = "Key\\032Light\\032Left._elg._tcp.local.";
    const RIGHT: &str = "Key\\032Light\\032Right._elg._tcp.local.";

    fn desk() -> Tracker {
        let mut tracker = Tracker::default();
        tracker.resolved(LEFT, "3C6A9D266B74");
        tracker.resolved(RIGHT, "3C6A9D25DCA7");
        tracker
    }

    #[test]
    fn a_healthy_desk_schedules_no_extra_queries() {
        let mut tracker = desk();
        assert_eq!(tracker.resolved(LEFT, "3C6A9D266B74"), Sighting::Again);
        assert_eq!(tracker.next_requery, None);
    }

    #[test]
    fn a_lapsed_record_is_a_hint_to_probe_and_requeries_promptly() {
        let mut tracker = desk();
        let now = Instant::now();
        assert_eq!(tracker.removed(LEFT, now).as_deref(), Some("3C6A9D266B74"));
        assert_eq!(tracker.next_requery, Some(now + REQUERY_PROMPTLY));
    }

    #[test]
    fn lamps_lapsing_together_share_one_requery() {
        let mut tracker = desk();
        let now = Instant::now();
        tracker.removed(LEFT, now);
        tracker.removed(RIGHT, now + Duration::from_millis(20));
        assert_eq!(tracker.next_requery, Some(now + REQUERY_PROMPTLY));
    }

    #[test]
    fn requeries_repeat_gently_until_the_record_returns() {
        let mut tracker = desk();
        let now = Instant::now();
        tracker.removed(LEFT, now);

        tracker.requeried(now + REQUERY_PROMPTLY);
        assert_eq!(tracker.next_requery, Some(now + REQUERY_PROMPTLY + REQUERY_INTERVAL));

        assert_eq!(tracker.resolved(LEFT, "3C6A9D266B74"), Sighting::Returned);
        assert_eq!(tracker.next_requery, None, "nothing is missing any more");
    }

    #[test]
    fn requeries_continue_while_any_lamp_is_still_missing() {
        let mut tracker = desk();
        let now = Instant::now();
        tracker.removed(LEFT, now);
        tracker.removed(RIGHT, now);

        tracker.resolved(LEFT, "3C6A9D266B74");
        assert!(tracker.next_requery.is_some(), "the right lamp has not come back");
        tracker.requeried(now);
        assert_eq!(tracker.next_requery, Some(now + REQUERY_INTERVAL));
    }

    #[test]
    fn a_rescan_on_a_healthy_desk_leaves_nothing_scheduled() {
        let mut tracker = desk();
        tracker.requeried(Instant::now());
        assert_eq!(tracker.next_requery, None);
    }

    #[test]
    fn a_renamed_light_is_not_reported_as_lapsed_when_its_old_name_ages_out() {
        let mut tracker = desk();
        let renamed = "Desk._elg._tcp.local.";
        assert_eq!(tracker.resolved(renamed, "3C6A9D266B74"), Sighting::Again);

        assert_eq!(tracker.removed(LEFT, Instant::now()), None);
        assert_eq!(tracker.next_requery, None);
    }

    #[test]
    fn a_removal_for_a_record_never_seen_is_ignored() {
        let mut tracker = desk();
        assert_eq!(tracker.removed("Stranger._elg._tcp.local.", Instant::now()), None);
        assert_eq!(tracker.next_requery, None);
    }

    #[test]
    fn instance_names_are_unescaped() {
        assert_eq!(
            instance_name("Elgato\\032Key\\032Light\\032Left._elg._tcp.local."),
            "Elgato Key Light Left"
        );
        assert_eq!(instance_name("Ring\\032Light._elg._tcp.local."), "Ring Light");
    }

    #[test]
    fn an_unescaped_name_survives_unchanged() {
        assert_eq!(instance_name("KeyLight._elg._tcp.local."), "KeyLight");
    }

    #[test]
    fn a_fullname_without_the_service_suffix_is_used_whole() {
        assert_eq!(instance_name("Weird Name"), "Weird Name");
    }

    #[test]
    fn literal_escapes_are_handled() {
        assert_eq!(unescape("Studio\\.One"), "Studio.One");
        assert_eq!(unescape("back\\\\slash"), "back\\slash");
    }

    #[test]
    fn a_trailing_backslash_does_not_panic() {
        assert_eq!(unescape("trailing\\"), "trailing");
        assert_eq!(unescape("short\\12"), "short12");
    }
}
