//! Noticing that the machine has moved to another network.
//!
//! A connection a network change killed is otherwise found only by TCP
//! keepalive, by the next command's timeout or by the end of an IDLE cycle —
//! minutes in which push is silent and an action waits. The machine's own
//! addresses are the signal: another Wi-Fi, a cable, a VPN coming up or going
//! down each changes them.
//!
//! Not every change counts. Loopback and link-local addresses say nothing
//! about the route to a mail server, and an IPv6 privacy address is replaced
//! every day on the same network, so IPv6 is compared by its network prefix.
//! Changes come in bursts — an interface goes down and up, each family
//! separately — so the watch waits for them to settle and then compares with
//! the network it last reported.
//!
//! Where Orb cannot watch the addresses itself — on iOS, without the
//! `desktop` feature — the platform reports a change through
//! [`report_change`] instead, and the watch passes it on the same way.

#[cfg(feature = "desktop")]
use std::collections::BTreeSet;
#[cfg(feature = "desktop")]
use std::net::IpAddr;
use std::sync::LazyLock;
#[cfg(feature = "desktop")]
use std::time::Duration;

#[cfg(feature = "desktop")]
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

/// The changes the platform has reported, to every running watch.
static REPORTED: LazyLock<broadcast::Sender<()>> = LazyLock::new(|| broadcast::channel(4).0);

/// Tell every watch that the machine has moved to another network.
pub fn report_change() {
    crate::logger::info("network: the platform reports another network; reconnecting");
    let _ = REPORTED.send(());
}

#[cfg(feature = "desktop")]
/// How long the addresses must stay put before a change counts.
const SETTLE: Duration = Duration::from_secs(3);

#[cfg(feature = "desktop")]
/// The part of an address that names the network the machine is on, or
/// `None` for one that names no route anywhere.
fn network_of(net: &if_watch::IpNet) -> Option<IpAddr> {
    let addr = net.addr();
    let local = match addr {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_link_local() || v4.is_unspecified(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    };
    if local {
        return None;
    }
    Some(match addr {
        IpAddr::V4(_) => addr,
        // The prefix, not the address: privacy addresses rotate within it.
        IpAddr::V6(_) => net.trunc().addr(),
    })
}

#[cfg(feature = "desktop")]
/// The networks a set of interface addresses puts the machine on.
fn networks<'a>(addresses: impl Iterator<Item = &'a if_watch::IpNet>) -> BTreeSet<IpAddr> {
    addresses.filter_map(network_of).collect()
}

#[cfg(feature = "desktop")]
/// Whether settling on `now`, having last been on `before`, is a move worth
/// reconnecting for. Going offline is not: nothing can reconnect until the
/// machine is back on a network, and that is the change reported.
fn moved(before: &BTreeSet<IpAddr>, now: &BTreeSet<IpAddr>) -> bool {
    !now.is_empty() && now != before
}

/// Watch the machine's addresses and send `()` on `changed` each time it has
/// settled on another network, until `shutdown`. Without a way to watch them
/// it says so in the log and returns: the timeouts still find dead
/// connections, only later.
#[cfg(feature = "desktop")]
pub async fn watch(changed: mpsc::UnboundedSender<()>, shutdown: CancellationToken) {
    let mut watcher = match if_watch::tokio::IfWatcher::new() {
        Ok(watcher) => watcher,
        Err(error) => {
            crate::logger::warn(&format!(
                "network: cannot watch the machine's addresses ({error}); \
                 a network change is noticed by the timeouts instead"
            ));
            return relay_reports(changed, shutdown).await;
        }
    };
    let mut reports = REPORTED.subscribe();
    let mut reported = networks(watcher.iter());
    let mut pending = false;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            Ok(()) = reports.recv() => {
                if changed.send(()).is_err() {
                    return;
                }
            }
            event = watcher.next() => match event {
                Some(Ok(_)) => pending = true,
                Some(Err(error)) => {
                    crate::logger::warn(&format!("network: the address watch stopped ({error})"));
                    return relay_reports(changed, shutdown).await;
                }
                None => return relay_reports(changed, shutdown).await,
            },
            _ = tokio::time::sleep(SETTLE), if pending => {
                pending = false;
                let now = networks(watcher.iter());
                if moved(&reported, &now) {
                    crate::logger::info(&format!(
                        "network: the machine is on another network ({} → {}); reconnecting",
                        describe(&reported),
                        describe(&now)
                    ));
                    if changed.send(()).is_err() {
                        return;
                    }
                }
                if !now.is_empty() {
                    reported = now;
                }
            }
        }
    }
}

/// Send `()` on `changed` for each change the platform reports, until
/// `shutdown`.
#[cfg(not(feature = "desktop"))]
pub async fn watch(changed: mpsc::UnboundedSender<()>, shutdown: CancellationToken) {
    relay_reports(changed, shutdown).await
}

/// Pass on what the platform reports, for when the addresses cannot be
/// watched.
async fn relay_reports(changed: mpsc::UnboundedSender<()>, shutdown: CancellationToken) {
    let mut reports = REPORTED.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            report = reports.recv() => match report {
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    if changed.send(()).is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

#[cfg(feature = "desktop")]
fn describe(networks: &BTreeSet<IpAddr>) -> String {
    if networks.is_empty() {
        return "offline".to_string();
    }
    networks
        .iter()
        .map(IpAddr::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;

    fn nets(list: &[&str]) -> Vec<if_watch::IpNet> {
        list.iter().map(|n| n.parse().unwrap()).collect()
    }

    /// The watch starts on this machine's real addresses, reports nothing
    /// while they stay put, and ends when asked.
    #[tokio::test]
    async fn the_watch_is_quiet_on_a_steady_network_and_stops_when_asked() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(watch(tx, shutdown.clone()));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(rx.try_recv().is_err(), "no move reported");
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("the watch ends")
            .unwrap();
    }

    #[test]
    fn only_routable_addresses_name_a_network() {
        let found = networks(
            nets(&[
                "127.0.0.1/8",
                "::1/128",
                "169.254.3.4/16",
                "fe80::1/64",
                "192.168.1.20/24",
                "2001:db8:1:2:aaaa:bbbb:cccc:dddd/64",
            ])
            .iter(),
        );
        assert_eq!(
            found,
            ["192.168.1.20", "2001:db8:1:2::"]
                .iter()
                .map(|a| a.parse().unwrap())
                .collect()
        );
    }

    #[test]
    fn a_rotated_privacy_address_is_the_same_network() {
        let before = networks(nets(&["2001:db8:1:2:1111:2222:3333:4444/64"]).iter());
        let after = networks(nets(&["2001:db8:1:2:5555:6666:7777:8888/64"]).iter());
        assert!(!moved(&before, &after));
    }

    #[test]
    fn another_network_is_a_move_and_going_offline_is_not() {
        let home = networks(nets(&["192.168.1.20/24"]).iter());
        let office = networks(nets(&["10.0.4.7/16"]).iter());
        let vpn = networks(nets(&["10.0.4.7/16", "172.16.9.2/32"]).iter());
        let offline = BTreeSet::new();
        assert!(moved(&home, &office));
        assert!(moved(&office, &vpn), "a VPN coming up changes the route");
        assert!(!moved(&home, &offline));
        assert!(!moved(&home, &home));
    }
}
