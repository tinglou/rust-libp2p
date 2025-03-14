use std::collections::HashSet;

use libp2p_core::Multiaddr;

use crate::behaviour::{ExpiredListenAddr, FromSwarm, NewListenAddr};

/// Utility struct for tracking the addresses a [`Swarm`](crate::Swarm) is listening on.
#[derive(Debug, Default, Clone)]
pub struct ListenAddresses {
    addresses: HashSet<Multiaddr>,
}

impl ListenAddresses {
    /// Returns an [`Iterator`] over all listen addresses.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Multiaddr> {
        self.addresses.iter()
    }

    /// Feed a [`FromSwarm`] event to this struct.
    ///
    /// Returns whether the event changed our set of listen addresses.
    pub fn on_swarm_event(&mut self, event: &FromSwarm) -> bool {
        match event {
            FromSwarm::NewListenAddr(NewListenAddr { addr, .. }) => {
                self.addresses.insert((*addr).clone())
            }
            FromSwarm::ExpiredListenAddr(ExpiredListenAddr { addr, .. }) => {
                self.addresses.remove(addr)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use libp2p_core::{multiaddr::Protocol, transport::ListenerId};

    use super::*;

    #[test]
    fn new_listen_addr_returns_correct_changed_value() {
        let mut addresses = ListenAddresses::default();

        let changed = addresses.on_swarm_event(&new_listen_addr());
        assert!(changed);

        let changed = addresses.on_swarm_event(&new_listen_addr());
        assert!(!changed)
    }

    #[test]
    fn expired_listen_addr_returns_correct_changed_value() {
        let mut addresses = ListenAddresses::default();
        addresses.on_swarm_event(&new_listen_addr());

        let changed = addresses.on_swarm_event(&expired_listen_addr());
        assert!(changed);

        let changed = addresses.on_swarm_event(&expired_listen_addr());
        assert!(!changed)
    }

    fn new_listen_addr() -> FromSwarm<'static> {
        FromSwarm::NewListenAddr(NewListenAddr {
            listener_id: ListenerId::next(),
            addr: &MEMORY_ADDR,
        })
    }

    fn expired_listen_addr() -> FromSwarm<'static> {
        FromSwarm::ExpiredListenAddr(ExpiredListenAddr {
            listener_id: ListenerId::next(),
            addr: &MEMORY_ADDR,
        })
    }

    static MEMORY_ADDR: std::sync::LazyLock<Multiaddr> =
        std::sync::LazyLock::new(|| Multiaddr::empty().with(Protocol::Memory(1000)));
}
