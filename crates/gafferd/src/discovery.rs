//! Finding lights on the network.
//!
//! Both earlier implementations parsed the DNS wire format by hand, and both
//! missed the same things: TTL expiry and goodbye packets (so an unplugged light
//! never disappeared, it just started failing), the cache-flush bit, IPv6, and
//! interface hotplug. `mdns-sd` handles all four, so this module is only a
//! translation layer: mDNS records in, [`Discovered`] out.
//!
//! The one piece of domain knowledge that stays here: lights are keyed on the
//! TXT `id=` field, which is the hardware MAC, **not** the mDNS instance name.
//! Renaming a light in Elgato's app changes the instance name, and keying on it
//! would make the same physical light appear as a second device.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use mdns_sd::{ScopedIp, ServiceDaemon, ServiceEvent};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// The mDNS service type Elgato Key Lights advertise.
pub const ELGATO_SERVICE: &str = "_elg._tcp.local.";

/// Fallback port if a light advertises an SRV record without one.
pub const DEFAULT_PORT: u16 = 9123;

/// How long an explicit re-probe waits for an answer.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(2);

/// A light seen on the network.
///
/// Every string here has already been canonicalised or sanitised: an mDNS
/// record is attacker-controlled, and this is the boundary where that stops
/// being true.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    /// Canonical hardware id, from the TXT `id=` field.
    pub id: String,
    /// mDNS instance fullname, for re-probing.
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
#[derive(Clone, Debug)]
pub enum DiscoveryEvent {
    Found(Box<Discovered>),
    /// A light went away — by goodbye packet or TTL expiry.
    Lost(String),
}

/// A running mDNS browse.
pub struct Discovery {
    daemon: ServiceDaemon,
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

        tokio::spawn(async move {
            // Removal events identify a service by fullname, but the rest of the
            // daemon speaks in hardware ids, so keep the mapping here.
            let mut ids: HashMap<String, String> = HashMap::new();

            while let Ok(event) = receiver.recv_async().await {
                let outgoing = match event {
                    ServiceEvent::ServiceResolved(service) => match resolve(&service) {
                        Some(found) => {
                            // mDNS re-resolves the same instance repeatedly, so
                            // announce only the first sighting; otherwise a
                            // perfectly stable network fills the journal with
                            // duplicate "discovered" lines.
                            let first_sighting =
                                ids.insert(found.fullname.clone(), found.id.clone()).is_none();
                            if first_sighting {
                                info!(id = %found.id, name = %found.name, addr = ?found.addr, "light discovered");
                            } else {
                                debug!(id = %found.id, addr = ?found.addr, "light re-resolved");
                            }
                            Some(DiscoveryEvent::Found(Box::new(found)))
                        }
                        None => {
                            debug!(fullname = %service.fullname, "ignoring unusable service record");
                            None
                        }
                    },
                    ServiceEvent::ServiceRemoved(_, fullname) => match ids.remove(&fullname) {
                        Some(id) => {
                            info!(%id, %fullname, "light went away");
                            Some(DiscoveryEvent::Lost(id))
                        }
                        None => None,
                    },
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
        });

        Ok(Self { daemon })
    }

    /// Actively re-probe known instances.
    ///
    /// The browse re-queries on its own schedule; this exists so `gaffer rescan`
    /// does something immediate rather than waiting for the next cycle.
    pub fn verify(&self, fullnames: &[String]) {
        for fullname in fullnames {
            if let Err(error) = self.daemon.verify(fullname.clone(), VERIFY_TIMEOUT) {
                debug!(%fullname, %error, "could not re-probe");
            }
        }
    }

    /// Stop the responder thread.
    pub fn shutdown(&self) {
        if let Err(error) = self.daemon.shutdown() {
            warn!(%error, "mDNS responder did not shut down cleanly");
        }
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
