// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

#![cfg(test)]

use futures::{future::poll_fn, prelude::*};
use futures_timer::Delay;
use libp2p_core::{
    multiaddr::{multiaddr, Protocol},
    multihash::Multihash,
    transport::MemoryTransport,
    upgrade, Transport,
};
use libp2p_identity as identity;
use libp2p_noise as noise;
use libp2p_swarm::{self as swarm, Swarm, SwarmEvent};
use libp2p_yamux as yamux;
use quickcheck::*;
use rand::{random, rngs::StdRng, thread_rng, Rng, SeedableRng};
use tokio::runtime::Runtime;

use super::*;
use crate::{
    record::{store::MemoryStore, Key},
    K_VALUE, PROTOCOL_NAME, SHA_256_MH,
};

type TestSwarm = Swarm<Behaviour<MemoryStore>>;

fn build_node() -> (Multiaddr, TestSwarm) {
    build_node_with_config(Default::default())
}

fn build_node_with_config(cfg: Config) -> (Multiaddr, TestSwarm) {
    let local_key = identity::Keypair::generate_ed25519();
    let local_public_key = local_key.public();
    let transport = MemoryTransport::default()
        .upgrade(upgrade::Version::V1)
        .authenticate(noise::Config::new(&local_key).unwrap())
        .multiplex(yamux::Config::default())
        .boxed();

    let local_id = local_public_key.to_peer_id();
    let store = MemoryStore::new(local_id);
    let behaviour = Behaviour::with_config(local_id, store, cfg);

    let mut swarm = Swarm::new(
        transport,
        behaviour,
        local_id,
        swarm::Config::with_tokio_executor(),
    );

    let address: Multiaddr = Protocol::Memory(random::<u64>()).into();
    swarm.listen_on(address.clone()).unwrap();
    swarm.add_external_address(address.clone());

    (address, swarm)
}

/// Builds swarms, each listening on a port. Does *not* connect the nodes together.
fn build_nodes(num: usize) -> Vec<(Multiaddr, TestSwarm)> {
    build_nodes_with_config(num, Default::default())
}

/// Builds swarms, each listening on a port. Does *not* connect the nodes together.
fn build_nodes_with_config(num: usize, cfg: Config) -> Vec<(Multiaddr, TestSwarm)> {
    (0..num)
        .map(|_| build_node_with_config(cfg.clone()))
        .collect()
}

fn build_connected_nodes(total: usize, step: usize) -> Vec<(Multiaddr, TestSwarm)> {
    build_connected_nodes_with_config(total, step, Default::default())
}

fn build_connected_nodes_with_config(
    total: usize,
    step: usize,
    cfg: Config,
) -> Vec<(Multiaddr, TestSwarm)> {
    let mut swarms = build_nodes_with_config(total, cfg);
    let swarm_ids: Vec<_> = swarms
        .iter()
        .map(|(addr, swarm)| (addr.clone(), *swarm.local_peer_id()))
        .collect();

    let mut i = 0;
    for (j, (addr, peer_id)) in swarm_ids.iter().enumerate().skip(1) {
        if i < swarm_ids.len() {
            swarms[i]
                .1
                .behaviour_mut()
                .add_address(peer_id, addr.clone());
        }
        if j % step == 0 {
            i += step;
        }
    }

    swarms
}

fn build_fully_connected_nodes_with_config(
    total: usize,
    cfg: Config,
) -> Vec<(Multiaddr, TestSwarm)> {
    let mut swarms = build_nodes_with_config(total, cfg);
    let swarm_addr_and_peer_id: Vec<_> = swarms
        .iter()
        .map(|(addr, swarm)| (addr.clone(), *swarm.local_peer_id()))
        .collect();

    for (_addr, swarm) in swarms.iter_mut() {
        for (addr, peer) in &swarm_addr_and_peer_id {
            swarm.behaviour_mut().add_address(peer, addr.clone());
        }
    }

    swarms
}

fn random_multihash() -> Multihash<64> {
    Multihash::wrap(SHA_256_MH, &thread_rng().gen::<[u8; 32]>()).unwrap()
}

#[derive(Clone, Debug)]
struct Seed([u8; 32]);

impl Arbitrary for Seed {
    fn arbitrary(g: &mut Gen) -> Seed {
        let seed = core::array::from_fn(|_| u8::arbitrary(g));
        Seed(seed)
    }
}

#[test]
fn bootstrap() {
    fn prop(seed: Seed) {
        let mut rng = StdRng::from_seed(seed.0);

        let num_total = rng.gen_range(2..20);
        // When looking for the closest node to a key, Kademlia considers
        // K_VALUE nodes to query at initialization. If `num_group` is larger
        // than K_VALUE the remaining locally known nodes will not be
        // considered. Given that no other node is aware of them, they would be
        // lost entirely. To prevent the above restrict `num_group` to be equal
        // or smaller than K_VALUE.
        let num_group = rng.gen_range(1..(num_total % K_VALUE.get()) + 2);

        let mut cfg = Config::new(PROTOCOL_NAME);
        // Disabling periodic bootstrap and automatic bootstrap to prevent the bootstrap from
        // triggering automatically.
        cfg.set_periodic_bootstrap_interval(None);
        cfg.set_automatic_bootstrap_throttle(None);
        if rng.gen() {
            cfg.disjoint_query_paths(true);
        }

        let mut swarms = build_connected_nodes_with_config(num_total, num_group, cfg)
            .into_iter()
            .map(|(_a, s)| s)
            .collect::<Vec<_>>();

        let swarm_ids: Vec<_> = swarms.iter().map(Swarm::local_peer_id).cloned().collect();

        let qid = swarms[0].behaviour_mut().bootstrap().unwrap();

        // Expected known peers
        let expected_known = swarm_ids.iter().skip(1).cloned().collect::<HashSet<_>>();
        let mut first = true;

        // Run test
        let rt = Runtime::new().unwrap();
        rt.block_on(poll_fn(move |ctx| {
            for (i, swarm) in swarms.iter_mut().enumerate() {
                loop {
                    match swarm.poll_next_unpin(ctx) {
                        Poll::Ready(Some(SwarmEvent::Behaviour(
                            Event::OutboundQueryProgressed {
                                id,
                                result: QueryResult::Bootstrap(Ok(ok)),
                                ..
                            },
                        ))) => {
                            assert_eq!(id, qid);
                            assert_eq!(i, 0);
                            if first {
                                // Bootstrapping must start with a self-lookup.
                                assert_eq!(ok.peer, swarm_ids[0]);
                            }
                            first = false;
                            if ok.num_remaining == 0 {
                                assert_eq!(
                                    swarm.behaviour_mut().queries.size(),
                                    0,
                                    "Expect no remaining queries when `num_remaining` is zero.",
                                );
                                let mut known = HashSet::new();
                                for b in swarm.behaviour_mut().kbuckets.iter() {
                                    for e in b.iter() {
                                        known.insert(*e.node.key.preimage());
                                    }
                                }
                                assert_eq!(expected_known, known);
                                return Poll::Ready(());
                            }
                        }
                        // Ignore any other event.
                        Poll::Ready(Some(_)) => (),
                        e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                        Poll::Pending => break,
                    }
                }
            }
            Poll::Pending
        }))
    }

    QuickCheck::new().tests(10).quickcheck(prop as fn(_) -> _)
}

#[test]
fn query_iter() {
    fn distances<K>(key: &kbucket::Key<K>, peers: Vec<PeerId>) -> Vec<Distance> {
        peers
            .into_iter()
            .map(kbucket::Key::from)
            .map(|k| k.distance(key))
            .collect()
    }

    fn run(rng: &mut impl Rng) {
        let num_total = rng.gen_range(2..20);
        let mut config = Config::new(PROTOCOL_NAME);
        // Disabling periodic bootstrap and automatic bootstrap to prevent the bootstrap from
        // triggering automatically.
        config.set_periodic_bootstrap_interval(None);
        config.set_automatic_bootstrap_throttle(None);
        let mut swarms = build_connected_nodes_with_config(num_total, 1, config)
            .into_iter()
            .map(|(_a, s)| s)
            .collect::<Vec<_>>();
        let swarm_ids: Vec<_> = swarms.iter().map(Swarm::local_peer_id).cloned().collect();

        // Ask the first peer in the list to search a random peer. The search should
        // propagate forwards through the list of peers.
        let search_target = PeerId::random();
        let search_target_key = kbucket::Key::from(search_target);
        let qid = swarms[0].behaviour_mut().get_closest_peers(search_target);

        match swarms[0].behaviour_mut().query(&qid) {
            Some(q) => match q.info() {
                QueryInfo::GetClosestPeers { key, step, .. } => {
                    assert_eq!(&key[..], search_target.to_bytes().as_slice());
                    assert_eq!(usize::from(step.count), 1);
                }
                i => panic!("Unexpected query info: {i:?}"),
            },
            None => panic!("Query not found: {qid:?}"),
        }

        // Set up expectations.
        let expected_swarm_id = swarm_ids[0];
        let expected_peer_ids: Vec<_> = swarm_ids.iter().skip(1).cloned().collect();
        let mut expected_distances = distances(&search_target_key, expected_peer_ids.clone());
        expected_distances.sort();

        // Run test
        let rt = Runtime::new().unwrap();
        rt.block_on(poll_fn(move |ctx| {
            for (i, swarm) in swarms.iter_mut().enumerate() {
                loop {
                    match swarm.poll_next_unpin(ctx) {
                        Poll::Ready(Some(SwarmEvent::Behaviour(
                            Event::OutboundQueryProgressed {
                                id,
                                result: QueryResult::GetClosestPeers(Ok(ok)),
                                ..
                            },
                        ))) => {
                            assert_eq!(id, qid);
                            assert_eq!(&ok.key[..], search_target.to_bytes().as_slice());
                            assert_eq!(swarm_ids[i], expected_swarm_id);
                            assert_eq!(swarm.behaviour_mut().queries.size(), 0);
                            let peer_ids =
                                ok.peers.into_iter().map(|p| p.peer_id).collect::<Vec<_>>();
                            assert!(expected_peer_ids.iter().all(|p| peer_ids.contains(p)));
                            let key = kbucket::Key::new(ok.key);
                            assert_eq!(expected_distances, distances(&key, peer_ids));
                            return Poll::Ready(());
                        }
                        // Ignore any other event.
                        Poll::Ready(Some(_)) => (),
                        e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                        Poll::Pending => break,
                    }
                }
            }
            Poll::Pending
        }))
    }

    let mut rng = thread_rng();
    for _ in 0..10 {
        run(&mut rng)
    }
}

#[test]
fn unresponsive_not_returned_direct() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    // Build one node. It contains fake addresses to non-existing nodes. We ask it to find a
    // random peer. We make sure that no fake address is returned.

    let mut swarms = build_nodes(1)
        .into_iter()
        .map(|(_a, s)| s)
        .collect::<Vec<_>>();

    // Add fake addresses.
    for _ in 0..10 {
        swarms[0]
            .behaviour_mut()
            .add_address(&PeerId::random(), Protocol::Udp(10u16).into());
    }

    // Ask first to search a random value.
    let search_target = PeerId::random();
    swarms[0].behaviour_mut().get_closest_peers(search_target);

    let rt = Runtime::new().unwrap();
    rt.block_on(poll_fn(move |ctx| {
        for swarm in &mut swarms {
            loop {
                match swarm.poll_next_unpin(ctx) {
                    Poll::Ready(Some(SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                        result: QueryResult::GetClosestPeers(Ok(ok)),
                        ..
                    }))) => {
                        assert_eq!(&ok.key[..], search_target.to_bytes().as_slice());
                        assert_eq!(ok.peers.len(), 0);
                        return Poll::Ready(());
                    }
                    // Ignore any other event.
                    Poll::Ready(Some(_)) => (),
                    e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                    Poll::Pending => break,
                }
            }
        }

        Poll::Pending
    }))
}

#[test]
fn unresponsive_not_returned_indirect() {
    // Build two nodes. Node #2 knows about node #1. Node #1 contains fake addresses to
    // non-existing nodes. We ask node #2 to find a random peer. We make sure that no fake address
    // is returned.

    let mut swarms = build_nodes(2);

    // Add fake addresses to first.
    for _ in 0..10 {
        swarms[0]
            .1
            .behaviour_mut()
            .add_address(&PeerId::random(), multiaddr![Udp(10u16)]);
    }

    // Connect second to first.
    let first_peer_id = *swarms[0].1.local_peer_id();
    let first_address = swarms[0].0.clone();
    swarms[1]
        .1
        .behaviour_mut()
        .add_address(&first_peer_id, first_address);

    // Drop the swarm addresses.
    let mut swarms = swarms
        .into_iter()
        .map(|(_addr, swarm)| swarm)
        .collect::<Vec<_>>();

    // Ask second to search a random value.
    let search_target = PeerId::random();
    swarms[1].behaviour_mut().get_closest_peers(search_target);

    let rt = Runtime::new().unwrap();
    rt.block_on(poll_fn(move |ctx| {
        for swarm in &mut swarms {
            loop {
                match swarm.poll_next_unpin(ctx) {
                    Poll::Ready(Some(SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                        result: QueryResult::GetClosestPeers(Ok(ok)),
                        ..
                    }))) => {
                        assert_eq!(&ok.key[..], search_target.to_bytes().as_slice());
                        assert_eq!(ok.peers.len(), 1);
                        assert_eq!(ok.peers[0].peer_id, first_peer_id);
                        return Poll::Ready(());
                    }
                    // Ignore any other event.
                    Poll::Ready(Some(_)) => (),
                    e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                    Poll::Pending => break,
                }
            }
        }

        Poll::Pending
    }))
}

// Test the result of get_closest_peers with different num_results
// Note that the result is capped after exceeds K_VALUE
#[test]
fn get_closest_with_different_num_results() {
    let k_value = K_VALUE.get();
    for replication_factor in [5, k_value / 2, k_value] {
        for num_results in k_value / 2..k_value * 2 {
            get_closest_with_different_num_results_inner(num_results, replication_factor)
        }
    }
}

fn get_closest_with_different_num_results_inner(num_results: usize, replication_factor: usize) {
    let k_value = K_VALUE.get();
    let num_of_nodes = 3 * k_value;
    let mut cfg = Config::new(PROTOCOL_NAME);
    cfg.set_replication_factor(NonZeroUsize::new(replication_factor).unwrap());
    let swarms = build_connected_nodes_with_config(num_of_nodes, replication_factor - 1, cfg);

    let mut swarms = swarms
        .into_iter()
        .map(|(_addr, swarm)| swarm)
        .collect::<Vec<_>>();

    // Ask first to search a random value.
    let search_target = PeerId::random();
    let Some(num_results_nonzero) = std::num::NonZeroUsize::new(num_results) else {
        panic!("Unexpected NonZeroUsize val of {num_results}");
    };
    swarms[0]
        .behaviour_mut()
        .get_n_closest_peers(search_target, num_results_nonzero);

    let rt = Runtime::new().unwrap();
    rt.block_on(poll_fn(move |ctx| {
        for swarm in &mut swarms {
            loop {
                match swarm.poll_next_unpin(ctx) {
                    Poll::Ready(Some(SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                        result: QueryResult::GetClosestPeers(Ok(ok)),
                        ..
                    }))) => {
                        assert_eq!(&ok.key[..], search_target.to_bytes().as_slice());
                        if num_results > k_value {
                            assert_eq!(ok.peers.len(), k_value, "Failed with replication_factor: {replication_factor}, num_results: {num_results}");
                        } else {
                            assert_eq!(ok.peers.len(), num_results, "Failed with replication_factor: {replication_factor}, num_results: {num_results}");
                        }

                        return Poll::Ready(());
                    }
                    // Ignore any other event.
                    Poll::Ready(Some(_)) => (),
                    e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                    Poll::Pending => break,
                }
            }
        }

        Poll::Pending
    }))
}

#[test]
fn get_record_not_found() {
    let mut swarms = build_nodes(3);

    let swarm_ids: Vec<_> = swarms
        .iter()
        .map(|(_addr, swarm)| *swarm.local_peer_id())
        .collect();

    let (second, third) = (swarms[1].0.clone(), swarms[2].0.clone());
    swarms[0]
        .1
        .behaviour_mut()
        .add_address(&swarm_ids[1], second);
    swarms[1]
        .1
        .behaviour_mut()
        .add_address(&swarm_ids[2], third);

    // Drop the swarm addresses.
    let mut swarms = swarms
        .into_iter()
        .map(|(_addr, swarm)| swarm)
        .collect::<Vec<_>>();

    let target_key = record::Key::from(random_multihash());
    let qid = swarms[0].behaviour_mut().get_record(target_key.clone());

    let rt = Runtime::new().unwrap();
    rt.block_on(poll_fn(move |ctx| {
        for swarm in &mut swarms {
            loop {
                match swarm.poll_next_unpin(ctx) {
                    Poll::Ready(Some(SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                        id,
                        result: QueryResult::GetRecord(Err(e)),
                        ..
                    }))) => {
                        assert_eq!(id, qid);
                        if let GetRecordError::NotFound { key, closest_peers } = e {
                            assert_eq!(key, target_key);
                            assert_eq!(closest_peers.len(), 2);
                            assert!(closest_peers.contains(&swarm_ids[1]));
                            assert!(closest_peers.contains(&swarm_ids[2]));
                            return Poll::Ready(());
                        } else {
                            panic!("Unexpected error result: {e:?}");
                        }
                    }
                    // Ignore any other event.
                    Poll::Ready(Some(_)) => (),
                    e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                    Poll::Pending => break,
                }
            }
        }

        Poll::Pending
    }))
}

/// A node joining a fully connected network via three (ALPHA_VALUE) bootnodes
/// should be able to put a record to the X closest nodes of the network where X
/// is equal to the configured replication factor.
#[test]
fn put_record() {
    fn prop(records: Vec<Record>, seed: Seed, filter_records: bool, drop_records: bool) {
        let mut rng = StdRng::from_seed(seed.0);
        let replication_factor =
            NonZeroUsize::new(rng.gen_range(1..(K_VALUE.get() / 2) + 1)).unwrap();
        // At least 4 nodes, 1 under test + 3 bootnodes.
        let num_total = usize::max(4, replication_factor.get() * 2);

        let mut config = Config::new(PROTOCOL_NAME);
        config.set_replication_factor(replication_factor);
        // Disabling periodic bootstrap and automatic bootstrap to prevent the bootstrap from
        // triggering automatically.
        config.set_periodic_bootstrap_interval(None);
        config.set_automatic_bootstrap_throttle(None);
        if rng.gen() {
            config.disjoint_query_paths(true);
        }

        if filter_records {
            config.set_record_filtering(StoreInserts::FilterBoth);
        }

        let mut swarms = {
            let mut fully_connected_swarms =
                build_fully_connected_nodes_with_config(num_total - 1, config.clone());

            let mut single_swarm = build_node_with_config(config);
            // Connect `single_swarm` to three bootnodes.
            for swarm in fully_connected_swarms.iter().take(3) {
                single_swarm
                    .1
                    .behaviour_mut()
                    .add_address(swarm.1.local_peer_id(), swarm.0.clone());
            }

            let mut swarms = vec![single_swarm];
            swarms.append(&mut fully_connected_swarms);

            // Drop the swarm addresses.
            swarms
                .into_iter()
                .map(|(_addr, swarm)| swarm)
                .collect::<Vec<_>>()
        };

        #[allow(clippy::mutable_key_type)] // False positive, we never modify `Bytes`.
        let records = records
            .into_iter()
            .take(num_total)
            .map(|mut r| {
                // We don't want records to expire prematurely, as they would
                // be removed from storage and no longer replicated, but we still
                // want to check that an explicitly set expiration is preserved.
                r.expires = r.expires.map(|t| t + Duration::from_secs(60));
                (r.key.clone(), r)
            })
            .collect::<HashMap<_, _>>();

        // Initiate put_record queries.
        let mut qids = HashSet::new();
        for r in records.values() {
            let qid = swarms[0]
                .behaviour_mut()
                .put_record(r.clone(), Quorum::All)
                .unwrap();
            match swarms[0].behaviour_mut().query(&qid) {
                Some(q) => match q.info() {
                    QueryInfo::PutRecord { phase, record, .. } => {
                        assert_eq!(phase, &PutRecordPhase::GetClosestPeers);
                        assert_eq!(record.key, r.key);
                        assert_eq!(record.value, r.value);
                        assert!(record.expires.is_some());
                        qids.insert(qid);
                    }
                    i => panic!("Unexpected query info: {i:?}"),
                },
                None => panic!("Query not found: {qid:?}"),
            }
        }

        // Each test run republishes all records once.
        let mut republished = false;
        // The accumulated results for one round of publishing.
        let mut results = Vec::new();

        let rt = Runtime::new().unwrap();
        rt.block_on(poll_fn(move |ctx| loop {
            // Poll all swarms until they are "Pending".
            for swarm in &mut swarms {
                loop {
                    match swarm.poll_next_unpin(ctx) {
                        Poll::Ready(Some(SwarmEvent::Behaviour(
                            Event::OutboundQueryProgressed {
                                id,
                                result: QueryResult::PutRecord(res),
                                stats,
                                step: index,
                            },
                        )))
                        | Poll::Ready(Some(SwarmEvent::Behaviour(
                            Event::OutboundQueryProgressed {
                                id,
                                result: QueryResult::RepublishRecord(res),
                                stats,
                                step: index,
                            },
                        ))) => {
                            assert!(qids.is_empty() || qids.remove(&id));
                            assert!(stats.duration().is_some());
                            assert!(stats.num_successes() >= replication_factor.get() as u32);
                            assert!(stats.num_requests() >= stats.num_successes());
                            assert_eq!(stats.num_failures(), 0);
                            assert_eq!(usize::from(index.count), 1);
                            assert!(index.last);
                            match res {
                                Err(e) => panic!("{e:?}"),
                                Ok(ok) => {
                                    assert!(records.contains_key(&ok.key));
                                    let record = swarm.behaviour_mut().store.get(&ok.key).unwrap();
                                    results.push(record.into_owned());
                                }
                            }
                        }
                        Poll::Ready(Some(SwarmEvent::Behaviour(Event::InboundRequest {
                            request: InboundRequest::PutRecord { record, .. },
                        }))) => {
                            if !drop_records {
                                if let Some(record) = record {
                                    assert_eq!(
                                        swarm.behaviour().record_filtering,
                                        StoreInserts::FilterBoth
                                    );
                                    // Accept the record
                                    swarm
                                        .behaviour_mut()
                                        .store_mut()
                                        .put(record)
                                        .expect("record is stored");
                                } else {
                                    assert_eq!(
                                        swarm.behaviour().record_filtering,
                                        StoreInserts::Unfiltered
                                    );
                                }
                            }
                        }
                        // Ignore any other event.
                        Poll::Ready(Some(_)) => (),
                        e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                        Poll::Pending => break,
                    }
                }
            }

            // All swarms are Pending and not enough results have been collected
            // so far, thus wait to be polled again for further progress.
            if results.len() != records.len() {
                return Poll::Pending;
            }

            // Consume the results, checking that each record was replicated
            // correctly to the closest peers to the key.
            while let Some(r) = results.pop() {
                let expected = records.get(&r.key).unwrap();

                assert_eq!(r.key, expected.key);
                assert_eq!(r.value, expected.value);
                assert_eq!(r.expires, expected.expires);
                assert_eq!(r.publisher, Some(*swarms[0].local_peer_id()));

                let key = kbucket::Key::new(r.key.clone());
                let mut expected = swarms
                    .iter()
                    .skip(1)
                    .map(Swarm::local_peer_id)
                    .cloned()
                    .collect::<Vec<_>>();
                expected.sort_by(|id1, id2| {
                    kbucket::Key::from(*id1)
                        .distance(&key)
                        .cmp(&kbucket::Key::from(*id2).distance(&key))
                });

                let expected = expected
                    .into_iter()
                    .take(replication_factor.get())
                    .collect::<HashSet<_>>();

                let actual = swarms
                    .iter()
                    .skip(1)
                    .filter_map(|swarm| {
                        if swarm.behaviour().store.get(key.preimage()).is_some() {
                            Some(*swarm.local_peer_id())
                        } else {
                            None
                        }
                    })
                    .collect::<HashSet<_>>();

                if swarms[0].behaviour().record_filtering != StoreInserts::Unfiltered
                    && drop_records
                {
                    assert_eq!(actual.len(), 0);
                } else {
                    assert_eq!(actual.len(), replication_factor.get());

                    let actual_not_expected =
                        actual.difference(&expected).collect::<Vec<&PeerId>>();
                    assert!(
                        actual_not_expected.is_empty(),
                        "Did not expect records to be stored on nodes {actual_not_expected:?}.",
                    );

                    let expected_not_actual =
                        expected.difference(&actual).collect::<Vec<&PeerId>>();
                    assert!(
                        expected_not_actual.is_empty(),
                        "Expected record to be stored on nodes {expected_not_actual:?}.",
                    );
                }
            }

            if republished {
                assert_eq!(
                    swarms[0].behaviour_mut().store.records().count(),
                    records.len()
                );
                assert_eq!(swarms[0].behaviour_mut().queries.size(), 0);
                for k in records.keys() {
                    swarms[0].behaviour_mut().store.remove(k);
                }
                assert_eq!(swarms[0].behaviour_mut().store.records().count(), 0);
                // All records have been republished, thus the test is complete.
                return Poll::Ready(());
            }

            // Tell the replication job to republish asap.
            swarms[0]
                .behaviour_mut()
                .put_record_job
                .as_mut()
                .unwrap()
                .asap(true);
            republished = true;
        }))
    }

    QuickCheck::new()
        .tests(4)
        .quickcheck(prop as fn(_, _, _, _) -> _)
}

#[test]
fn get_record() {
    let mut swarms = build_nodes(3);

    // Let first peer know of second peer and second peer know of third peer.
    for i in 0..2 {
        let (peer_id, address) = (
            *Swarm::local_peer_id(&swarms[i + 1].1),
            swarms[i + 1].0.clone(),
        );
        swarms[i].1.behaviour_mut().add_address(&peer_id, address);
    }

    // Drop the swarm addresses.
    let mut swarms = swarms
        .into_iter()
        .map(|(_addr, swarm)| swarm)
        .collect::<Vec<_>>();

    let record = Record::new(random_multihash(), vec![4, 5, 6]);

    swarms[2].behaviour_mut().store.put(record.clone()).unwrap();
    let qid = swarms[0].behaviour_mut().get_record(record.key.clone());

    let rt = Runtime::new().unwrap();
    rt.block_on(poll_fn(move |ctx| {
        for swarm in &mut swarms {
            loop {
                match swarm.poll_next_unpin(ctx) {
                    Poll::Ready(Some(SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                        id,
                        result: QueryResult::GetRecord(Ok(r)),
                        step: ProgressStep { count, last },
                        ..
                    }))) => {
                        assert_eq!(id, qid);
                        if usize::from(count) == 1 {
                            assert!(!last);
                            assert!(matches!(r, GetRecordOk::FoundRecord(_)));
                            if let GetRecordOk::FoundRecord(r) = r {
                                assert_eq!(r.record, record);
                            }
                        } else if last {
                            assert_eq!(usize::from(count), 2);
                            assert!(matches!(
                                r,
                                GetRecordOk::FinishedWithNoAdditionalRecord { .. }
                            ));
                        }
                        return Poll::Ready(());
                    }
                    // Ignore any other event.
                    Poll::Ready(Some(_)) => (),
                    e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                    Poll::Pending => break,
                }
            }
        }

        Poll::Pending
    }))
}

#[test]
fn get_record_many() {
    // TODO: Randomise
    let num_nodes = 12;
    let mut swarms = build_connected_nodes(num_nodes, 3)
        .into_iter()
        .map(|(_addr, swarm)| swarm)
        .collect::<Vec<_>>();
    let num_results = 10;

    let record = Record::new(random_multihash(), vec![4, 5, 6]);

    for swarm in swarms.iter_mut().take(num_nodes) {
        swarm.behaviour_mut().store.put(record.clone()).unwrap();
    }

    let quorum = Quorum::N(NonZeroUsize::new(num_results).unwrap());
    let qid = swarms[0].behaviour_mut().get_record(record.key.clone());

    let rt = Runtime::new().unwrap();
    rt.block_on(poll_fn(move |ctx| {
        for (i, swarm) in swarms.iter_mut().enumerate() {
            let mut records = Vec::new();
            let quorum = quorum.eval(swarm.behaviour().queries.config().replication_factor);
            loop {
                if i == 0 && records.len() >= quorum.get() {
                    swarm.behaviour_mut().query_mut(&qid).unwrap().finish();
                }
                match swarm.poll_next_unpin(ctx) {
                    Poll::Ready(Some(SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                        id,
                        result: QueryResult::GetRecord(Ok(r)),
                        step: ProgressStep { count: _, last },
                        ..
                    }))) => {
                        assert_eq!(id, qid);
                        if let GetRecordOk::FoundRecord(r) = r {
                            assert_eq!(r.record, record);
                            records.push(r);
                        }

                        if last {
                            return Poll::Ready(());
                        }
                    }
                    // Ignore any other event.
                    Poll::Ready(Some(_)) => (),
                    e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                    Poll::Pending => break,
                }
            }
        }
        Poll::Pending
    }))
}

/// A node joining a fully connected network via three (ALPHA_VALUE) bootnodes
/// should be able to add itself as a provider to the X closest nodes of the
/// network where X is equal to the configured replication factor.
#[test]
fn add_provider() {
    fn prop(keys: Vec<record::Key>, seed: Seed) {
        let mut rng = StdRng::from_seed(seed.0);
        let replication_factor =
            NonZeroUsize::new(rng.gen_range(1..(K_VALUE.get() / 2) + 1)).unwrap();
        // At least 4 nodes, 1 under test + 3 bootnodes.
        let num_total = usize::max(4, replication_factor.get() * 2);

        let mut config = Config::new(PROTOCOL_NAME);
        config.set_replication_factor(replication_factor);
        // Disabling periodic bootstrap and automatic bootstrap to prevent the bootstrap from
        // triggering automatically.
        config.set_periodic_bootstrap_interval(None);
        config.set_automatic_bootstrap_throttle(None);
        if rng.gen() {
            config.disjoint_query_paths(true);
        }

        let mut swarms = {
            let mut fully_connected_swarms =
                build_fully_connected_nodes_with_config(num_total - 1, config.clone());

            let mut single_swarm = build_node_with_config(config);
            // Connect `single_swarm` to three bootnodes.
            for swarm in fully_connected_swarms.iter().take(3) {
                single_swarm
                    .1
                    .behaviour_mut()
                    .add_address(swarm.1.local_peer_id(), swarm.0.clone());
            }

            let mut swarms = vec![single_swarm];
            swarms.append(&mut fully_connected_swarms);

            // Drop addresses before returning.
            swarms
                .into_iter()
                .map(|(_addr, swarm)| swarm)
                .collect::<Vec<_>>()
        };

        #[allow(clippy::mutable_key_type)] // False positive, we never modify `Bytes`.
        let keys: HashSet<_> = keys.into_iter().take(num_total).collect();

        // Each test run publishes all records twice.
        let mut published = false;
        let mut republished = false;
        // The accumulated results for one round of publishing.
        let mut results = Vec::new();

        // Initiate the first round of publishing.
        let mut qids = HashSet::new();
        for k in &keys {
            let qid = swarms[0]
                .behaviour_mut()
                .start_providing(k.clone())
                .unwrap();
            qids.insert(qid);
        }

        let rt = Runtime::new().unwrap();
        rt.block_on(poll_fn(move |ctx| loop {
            // Poll all swarms until they are "Pending".
            for swarm in &mut swarms {
                loop {
                    match swarm.poll_next_unpin(ctx) {
                        Poll::Ready(Some(SwarmEvent::Behaviour(
                            Event::OutboundQueryProgressed {
                                id,
                                result: QueryResult::StartProviding(res),
                                ..
                            },
                        )))
                        | Poll::Ready(Some(SwarmEvent::Behaviour(
                            Event::OutboundQueryProgressed {
                                id,
                                result: QueryResult::RepublishProvider(res),
                                ..
                            },
                        ))) => {
                            assert!(qids.is_empty() || qids.remove(&id));
                            match res {
                                Err(e) => panic!("{e:?}"),
                                Ok(ok) => {
                                    assert!(keys.contains(&ok.key));
                                    results.push(ok.key);
                                }
                            }
                        }
                        // Ignore any other event.
                        Poll::Ready(Some(_)) => (),
                        e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                        Poll::Pending => break,
                    }
                }
            }

            if results.len() == keys.len() {
                // All requests have been sent for one round of publishing.
                published = true
            }

            if !published {
                // Still waiting for all requests to be sent for one round
                // of publishing.
                return Poll::Pending;
            }

            // A round of publishing is complete. Consume the results, checking that
            // each key was published to the `replication_factor` closest peers.
            while let Some(key) = results.pop() {
                // Collect the nodes that have a provider record for `key`.
                let actual = swarms
                    .iter()
                    .skip(1)
                    .filter_map(|swarm| {
                        if swarm.behaviour().store.providers(&key).len() == 1 {
                            Some(*Swarm::local_peer_id(swarm))
                        } else {
                            None
                        }
                    })
                    .collect::<HashSet<_>>();

                if actual.len() != replication_factor.get() {
                    // Still waiting for some nodes to process the request.
                    results.push(key);
                    return Poll::Pending;
                }

                let mut expected = swarms
                    .iter()
                    .skip(1)
                    .map(Swarm::local_peer_id)
                    .cloned()
                    .collect::<Vec<_>>();
                let kbucket_key = kbucket::Key::new(key);
                expected.sort_by(|id1, id2| {
                    kbucket::Key::from(*id1)
                        .distance(&kbucket_key)
                        .cmp(&kbucket::Key::from(*id2).distance(&kbucket_key))
                });

                let expected = expected
                    .into_iter()
                    .take(replication_factor.get())
                    .collect::<HashSet<_>>();

                assert_eq!(actual, expected);
            }

            // One round of publishing is complete.
            assert!(results.is_empty());
            for swarm in &swarms {
                assert_eq!(swarm.behaviour().queries.size(), 0);
            }

            if republished {
                assert_eq!(
                    swarms[0].behaviour_mut().store.provided().count(),
                    keys.len()
                );
                for k in &keys {
                    swarms[0].behaviour_mut().stop_providing(k);
                }
                assert_eq!(swarms[0].behaviour_mut().store.provided().count(), 0);
                // All records have been republished, thus the test is complete.
                return Poll::Ready(());
            }

            // Initiate the second round of publishing by telling the
            // periodic provider job to run asap.
            swarms[0]
                .behaviour_mut()
                .add_provider_job
                .as_mut()
                .unwrap()
                .asap();
            published = false;
            republished = true;
        }))
    }

    QuickCheck::new().tests(3).quickcheck(prop as fn(_, _))
}

/// User code should be able to start queries beyond the internal
/// query limit for background jobs. Originally this even produced an
/// arithmetic overflow, see https://github.com/libp2p/rust-libp2p/issues/1290.
#[test]
fn exceed_jobs_max_queries() {
    let (_addr, mut swarm) = build_node();
    let num = JOBS_MAX_QUERIES + 1;
    for _ in 0..num {
        swarm.behaviour_mut().get_closest_peers(PeerId::random());
    }

    assert_eq!(swarm.behaviour_mut().queries.size(), num);

    let rt = Runtime::new().unwrap();
    rt.block_on(poll_fn(move |ctx| {
        for _ in 0..num {
            // There are no other nodes, so the queries finish instantly.
            loop {
                if let Poll::Ready(Some(e)) = swarm.poll_next_unpin(ctx) {
                    match e {
                        SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                            result: QueryResult::GetClosestPeers(Ok(r)),
                            ..
                        }) => break assert!(r.peers.is_empty()),
                        SwarmEvent::Behaviour(Event::ModeChanged { .. }) => {}
                        SwarmEvent::Behaviour(e) => panic!("Unexpected event: {e:?}"),
                        _ => {}
                    }
                } else {
                    panic!("Expected event")
                }
            }
        }
        Poll::Ready(())
    }))
}

#[test]
fn exp_decr_expiration_overflow() {
    fn prop_no_panic(ttl: Duration, factor: u32) {
        exp_decrease(ttl, factor);
    }

    // Right shifting a u64 by >63 results in a panic.
    prop_no_panic(Config::new(PROTOCOL_NAME).record_ttl.unwrap(), 64);

    quickcheck(prop_no_panic as fn(_, _))
}

#[test]
fn disjoint_query_does_not_finish_before_all_paths_did() {
    let mut config = Config::new(PROTOCOL_NAME);
    config.disjoint_query_paths(true);
    // I.e. setting the amount disjoint paths to be explored to 2.
    config.set_parallelism(NonZeroUsize::new(2).unwrap());
    // Disabling periodic bootstrap and automatic bootstrap to prevent the bootstrap from triggering
    // automatically.
    config.set_periodic_bootstrap_interval(None);
    config.set_automatic_bootstrap_throttle(None);

    let mut alice = build_node_with_config(config);
    let mut trudy = build_node(); // Trudy the intrudor, an adversary.
    let mut bob = build_node();

    let key = Key::from(
        Multihash::<64>::wrap(SHA_256_MH, &thread_rng().gen::<[u8; 32]>())
            .expect("32 array to fit into 64 byte multihash"),
    );
    let record_bob = Record::new(key.clone(), b"bob".to_vec());
    let record_trudy = Record::new(key.clone(), b"trudy".to_vec());

    // Make `bob` and `trudy` aware of their version of the record searched by
    // `alice`.
    bob.1.behaviour_mut().store.put(record_bob.clone()).unwrap();
    trudy.1.behaviour_mut().store.put(record_trudy).unwrap();

    // Make `trudy` and `bob` known to `alice`.
    alice
        .1
        .behaviour_mut()
        .add_address(trudy.1.local_peer_id(), trudy.0.clone());
    alice
        .1
        .behaviour_mut()
        .add_address(bob.1.local_peer_id(), bob.0.clone());

    // Drop the swarm addresses.
    let (mut alice, mut bob, mut trudy) = (alice.1, bob.1, trudy.1);

    // Have `alice` query the Dht for `key` with a quorum of 1.
    alice.behaviour_mut().get_record(key);

    // The default peer timeout is 10 seconds. Choosing 1 seconds here should
    // give enough head room to prevent connections to `bob` to time out.
    let mut before_timeout = Delay::new(Duration::from_secs(1));

    // Poll only `alice` and `trudy` expecting `alice` not yet to return a query
    // result as it is not able to connect to `bob` just yet.
    let addr_trudy = *Swarm::local_peer_id(&trudy);
    let rt = Runtime::new().unwrap();
    rt.block_on(poll_fn(|ctx| {
        for (i, swarm) in [&mut alice, &mut trudy].iter_mut().enumerate() {
            loop {
                match swarm.poll_next_unpin(ctx) {
                    Poll::Ready(Some(SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                        result: QueryResult::GetRecord(result),
                        step,
                        ..
                    }))) => {
                        if i != 0 {
                            panic!("Expected `QueryResult` from Alice.")
                        }
                        if step.last {
                            panic!(
                                "Expected query not to finish until all \
                                 disjoint paths have been explored.",
                            );
                        }
                        match result {
                            Ok(GetRecordOk::FoundRecord(r)) => {
                                assert_eq!(r.peer, Some(addr_trudy));
                            }
                            Ok(_) => {}
                            Err(e) => panic!("{e:?}"),
                        }
                    }
                    // Ignore any other event.
                    Poll::Ready(Some(_)) => (),
                    Poll::Ready(None) => panic!("Expected Kademlia behaviour not to finish."),
                    Poll::Pending => break,
                }
            }
        }

        // Make sure not to wait until connections to `bob` time out.
        before_timeout.poll_unpin(ctx)
    }));

    // Make sure `alice` has exactly one query with `trudy`'s record only.
    assert_eq!(1, alice.behaviour().queries.iter().count());

    alice
        .behaviour()
        .queries
        .iter()
        .for_each(|q| match &q.info {
            QueryInfo::GetRecord { step, .. } => {
                assert_eq!(usize::from(step.count), 2);
            }
            i => panic!("Unexpected query info: {i:?}"),
        });

    // Poll `alice` and `bob` expecting `alice` to return a successful query
    // result as it is now able to explore the second disjoint path.
    let records = rt.block_on(poll_fn(|ctx| {
        let mut records = Vec::new();
        for (i, swarm) in [&mut alice, &mut bob].iter_mut().enumerate() {
            loop {
                match swarm.poll_next_unpin(ctx) {
                    Poll::Ready(Some(SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                        result: QueryResult::GetRecord(result),
                        step,
                        ..
                    }))) => {
                        if i != 0 {
                            panic!("Expected `QueryResult` from Alice.")
                        }
                        match result {
                            Ok(ok) => {
                                if let GetRecordOk::FoundRecord(record) = ok {
                                    records.push(record);
                                }
                                if records.len() == 1 {
                                    return Poll::Ready(records);
                                }
                                if step.last {
                                    break;
                                }
                            }
                            Err(e) => unreachable!("{:?}", e),
                        }
                    }
                    // Ignore any other event.
                    Poll::Ready(Some(_)) => (),
                    Poll::Ready(None) => panic!("Expected Kademlia behaviour not to finish.",),
                    Poll::Pending => break,
                }
            }
        }

        Poll::Pending
    }));

    assert_eq!(1, records.len());
    assert!(records.contains(&PeerRecord {
        peer: Some(*Swarm::local_peer_id(&bob)),
        record: record_bob,
    }));
}

/// Tests that peers are not automatically inserted into
/// the routing table with `BucketInserts::Manual`.
#[test]
fn manual_bucket_inserts() {
    let mut cfg = Config::new(PROTOCOL_NAME);
    cfg.set_kbucket_inserts(BucketInserts::Manual);
    // 1 -> 2 -> [3 -> ...]
    let mut swarms = build_connected_nodes_with_config(3, 1, cfg);
    // The peers and their addresses for which we expect `RoutablePeer` events.
    let mut expected = swarms
        .iter()
        .skip(2)
        .map(|(a, s)| {
            let pid = *Swarm::local_peer_id(s);
            let addr = a.clone().with(Protocol::P2p(pid));
            (addr, pid)
        })
        .collect::<HashMap<_, _>>();
    // We collect the peers for which a `RoutablePeer` event
    // was received in here to check at the end of the test
    // that none of them was inserted into a bucket.
    let mut routable = Vec::new();
    // Start an iterative query from the first peer.
    swarms[0]
        .1
        .behaviour_mut()
        .get_closest_peers(PeerId::random());
    let rt = Runtime::new().unwrap();
    rt.block_on(poll_fn(move |ctx| {
        for (_, swarm) in swarms.iter_mut() {
            loop {
                match swarm.poll_next_unpin(ctx) {
                    Poll::Ready(Some(SwarmEvent::Behaviour(Event::RoutablePeer {
                        peer,
                        address,
                    }))) => {
                        assert_eq!(peer, expected.remove(&address).expect("Missing address"));
                        routable.push(peer);
                        if expected.is_empty() {
                            for peer in routable.iter() {
                                let bucket = swarm.behaviour_mut().kbucket(*peer).unwrap();
                                assert!(bucket.iter().all(|e| e.node.key.preimage() != peer));
                            }
                            return Poll::Ready(());
                        }
                    }
                    Poll::Ready(..) => {}
                    Poll::Pending => break,
                }
            }
        }
        Poll::Pending
    }));
}

#[test]
fn network_behaviour_on_address_change() {
    let local_peer_id = PeerId::random();

    let remote_peer_id = PeerId::random();
    let connection_id = ConnectionId::new_unchecked(0);
    let old_address: Multiaddr = Protocol::Memory(1).into();
    let new_address: Multiaddr = Protocol::Memory(2).into();

    let mut kademlia = Behaviour::new(local_peer_id, MemoryStore::new(local_peer_id));

    let endpoint = ConnectedPoint::Dialer {
        address: old_address.clone(),
        role_override: Endpoint::Dialer,
        port_use: PortUse::Reuse,
    };

    // Mimic a connection being established.
    kademlia.on_swarm_event(FromSwarm::ConnectionEstablished(ConnectionEstablished {
        peer_id: remote_peer_id,
        connection_id,
        endpoint: &endpoint,
        failed_addresses: &[],
        other_established: 0,
    }));

    // At this point the remote is not yet known to support the
    // configured protocol name, so the peer is not yet in the
    // local routing table and hence no addresses are known.
    assert!(kademlia
        .handle_pending_outbound_connection(
            connection_id,
            Some(remote_peer_id),
            &[],
            Endpoint::Dialer
        )
        .unwrap()
        .is_empty());

    // Mimic the connection handler confirming the protocol for
    // the test connection, so that the peer is added to the routing table.
    kademlia.on_connection_handler_event(
        remote_peer_id,
        connection_id,
        HandlerEvent::ProtocolConfirmed { endpoint },
    );

    assert_eq!(
        vec![old_address.clone()],
        kademlia
            .handle_pending_outbound_connection(
                connection_id,
                Some(remote_peer_id),
                &[],
                Endpoint::Dialer
            )
            .unwrap(),
    );

    kademlia.on_swarm_event(FromSwarm::AddressChange(AddressChange {
        peer_id: remote_peer_id,
        connection_id,
        old: &ConnectedPoint::Dialer {
            address: old_address,
            role_override: Endpoint::Dialer,
            port_use: PortUse::Reuse,
        },
        new: &ConnectedPoint::Dialer {
            address: new_address.clone(),
            role_override: Endpoint::Dialer,
            port_use: PortUse::Reuse,
        },
    }));

    assert_eq!(
        vec![new_address],
        kademlia
            .handle_pending_outbound_connection(
                connection_id,
                Some(remote_peer_id),
                &[],
                Endpoint::Dialer
            )
            .unwrap(),
    );
}

#[test]
fn get_providers_single() {
    fn prop(key: record::Key) {
        let (_, mut single_swarm) = build_node();
        single_swarm
            .behaviour_mut()
            .start_providing(key.clone())
            .expect("could not provide");

        let rt = Runtime::new().unwrap();
        rt.block_on(async {
            match single_swarm.next().await.unwrap() {
                SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                    result: QueryResult::StartProviding(Ok(_)),
                    ..
                }) => {}
                SwarmEvent::Behaviour(Event::ModeChanged { .. }) => {}
                SwarmEvent::Behaviour(e) => panic!("Unexpected event: {e:?}"),
                _ => {}
            }
        });

        let query_id = single_swarm.behaviour_mut().get_providers(key);

        rt.block_on(async {
            loop {
                match single_swarm.next().await.unwrap() {
                    SwarmEvent::Behaviour(Event::OutboundQueryProgressed {
                        id,
                        result: QueryResult::GetProviders(Ok(ok)),
                        step: index,
                        ..
                    }) if id == query_id => {
                        if index.last {
                            assert!(matches!(
                                ok,
                                GetProvidersOk::FinishedWithNoAdditionalRecord { .. }
                            ));
                            break;
                        } else {
                            assert!(matches!(ok, GetProvidersOk::FoundProviders { .. }));
                            if let GetProvidersOk::FoundProviders { providers, .. } = ok {
                                assert_eq!(providers.len(), 1);
                                assert!(providers.contains(single_swarm.local_peer_id()));
                            }
                        }
                    }
                    SwarmEvent::Behaviour(e) => panic!("Unexpected event: {e:?}"),
                    _ => {}
                }
            }
        });
    }
    QuickCheck::new().tests(10).quickcheck(prop as fn(_))
}

fn get_providers_limit<const N: usize>() {
    fn prop<const N: usize>(key: record::Key) {
        let mut swarms = build_nodes(3);

        // Let first peer know of second peer and second peer know of third peer.
        for i in 0..2 {
            let (peer_id, address) = (
                *Swarm::local_peer_id(&swarms[i + 1].1),
                swarms[i + 1].0.clone(),
            );
            swarms[i].1.behaviour_mut().add_address(&peer_id, address);
        }

        // Drop the swarm addresses.
        let mut swarms = swarms
            .into_iter()
            .map(|(_addr, swarm)| swarm)
            .collect::<Vec<_>>();

        // Provide the content on peer 2 and 3.
        for swarm in swarms.iter_mut().take(3).skip(1) {
            swarm
                .behaviour_mut()
                .start_providing(key.clone())
                .expect("could not provide");
        }

        // Query with expecting a single provider.
        let query_id = swarms[0].behaviour_mut().get_providers(key.clone());

        let mut all_providers: Vec<PeerId> = vec![];

        let rt = Runtime::new().unwrap();
        rt.block_on(poll_fn(move |ctx| {
            for (i, swarm) in swarms.iter_mut().enumerate() {
                loop {
                    match swarm.poll_next_unpin(ctx) {
                        Poll::Ready(Some(SwarmEvent::Behaviour(
                            Event::OutboundQueryProgressed {
                                id,
                                result: QueryResult::GetProviders(Ok(ok)),
                                step: index,
                                ..
                            },
                        ))) if i == 0 && id == query_id => {
                            if index.last {
                                assert!(matches!(
                                    ok,
                                    GetProvidersOk::FinishedWithNoAdditionalRecord { .. }
                                ));
                                assert_eq!(all_providers.len(), N);
                                return Poll::Ready(());
                            } else {
                                assert!(matches!(ok, GetProvidersOk::FoundProviders { .. }));
                                if let GetProvidersOk::FoundProviders {
                                    key: found_key,
                                    providers,
                                } = ok
                                {
                                    // There are a total of 2 providers.
                                    assert_eq!(key, found_key);
                                    for provider in &providers {
                                        // Providers should be either 2 or 3
                                        assert_ne!(swarm.local_peer_id(), provider);
                                    }
                                    all_providers.extend(providers);

                                    // If we have all providers, finish.
                                    if all_providers.len() == N {
                                        swarm.behaviour_mut().query_mut(&id).unwrap().finish();
                                    }
                                }
                                return Poll::Ready(());
                            }
                        }
                        Poll::Ready(..) => {}
                        Poll::Pending => break,
                    }
                }
            }
            Poll::Pending
        }));
    }

    QuickCheck::new().tests(10).quickcheck(prop::<N> as fn(_))
}

#[test]
fn get_providers_limit_n_1() {
    get_providers_limit::<1>();
}

#[test]
fn get_providers_limit_n_2() {
    get_providers_limit::<2>();
}

#[test]
fn get_providers_limit_n_5() {
    get_providers_limit::<5>();
}

// Test that nodes respond with K amount of peers even when replication factor is set lower than K.
#[test]
fn get_closest_peers_should_return_up_to_k_peers() {
    let k_value = K_VALUE.get();

    // Rplication factor should not influence the amount of peers returned in `GetClosestPeers`.
    for replication_factor in 5..k_value + 1 {
        // Should be enough nodes for every node to have >= K nodes in their RT.
        let num_of_nodes = 3 * k_value;

        let mut cfg = Config::new(PROTOCOL_NAME);
        cfg.set_replication_factor(NonZeroUsize::new(replication_factor).unwrap());

        let swarms = build_connected_nodes_with_config(num_of_nodes, replication_factor - 1, cfg);
        let mut swarms = swarms
            .into_iter()
            .map(|(_addr, swarm)| swarm)
            .collect::<Vec<_>>();

        // Ask first node to search for a random peer.
        let search_target = PeerId::random();
        swarms[0].behaviour_mut().get_closest_peers(search_target);

        let rt = Runtime::new().unwrap();
        rt.block_on(poll_fn(move |ctx| {
            for swarm in &mut swarms {
                loop {
                    match swarm.poll_next_unpin(ctx) {
                        Poll::Ready(Some(SwarmEvent::Behaviour(
                            Event::OutboundQueryProgressed {
                                result: QueryResult::GetClosestPeers(Ok(ok)),
                                ..
                            },
                        ))) => {
                            assert_eq!(&ok.key[..], search_target.to_bytes().as_slice());
                            // Verify that we get K_VALUE amount of peers even with lower
                            // replication factor.
                            assert_eq!(
                                ok.peers.len(),
                                k_value,
                                "Expected K_VALUE ({}) peers but got {}",
                                k_value,
                                ok.peers.len()
                            );
                            return Poll::Ready(());
                        }
                        // Ignore any other event.
                        Poll::Ready(Some(_)) => (),
                        e @ Poll::Ready(_) => panic!("Unexpected return value: {e:?}"),
                        Poll::Pending => break,
                    }
                }
            }
            Poll::Pending
        }))
    }
}
