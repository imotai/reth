//! Testing gossiping of transactions.
use alloy_consensus::TxLegacy;
use alloy_primitives::{Signature, U256};
use futures::StreamExt;
use reth_ethereum_primitives::TransactionSigned;
use reth_network::{
    test_utils::{NetworkEventStream, Testnet},
    transactions::config::TransactionPropagationKind,
    NetworkEvent, NetworkEventListenerProvider, Peers,
};
use reth_network_api::{events::PeerEvent, PeerKind, PeersInfo};
use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
use reth_transaction_pool::{
    test_utils::TransactionGenerator, AddedTransactionOutcome, PoolTransaction, TransactionPool,
};
use std::sync::Arc;
use tokio::join;

#[tokio::test(flavor = "multi_thread")]
async fn test_tx_gossip() {
    reth_tracing::init_test_tracing();

    let provider = MockEthProvider::default();
    let net = Testnet::create_with(2, provider.clone()).await;

    // install request handlers
    let net = net.with_eth_pool();
    let handle = net.spawn();
    // connect all the peers
    handle.connect_peers().await;

    let peer0 = &handle.peers()[0];
    let peer1 = &handle.peers()[1];

    let peer0_pool = peer0.pool().unwrap();
    let mut peer0_tx_listener = peer0.pool().unwrap().pending_transactions_listener();
    let mut peer1_tx_listener = peer1.pool().unwrap().pending_transactions_listener();

    let mut tx_gen = TransactionGenerator::new(rand::rng());
    let tx = tx_gen.gen_eip1559_pooled();

    // ensure the sender has balance
    let sender = tx.sender();
    provider.add_account(sender, ExtendedAccount::new(0, U256::from(100_000_000)));

    // insert pending tx in peer0's pool
    let AddedTransactionOutcome { hash, .. } =
        peer0_pool.add_external_transaction(tx).await.unwrap();

    let inserted = peer0_tx_listener.recv().await.unwrap();
    assert_eq!(inserted, hash);

    // ensure tx is gossiped to peer1
    let received = peer1_tx_listener.recv().await.unwrap();
    assert_eq!(received, hash);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tx_propagation_policy_trusted_only() {
    reth_tracing::init_test_tracing();

    let provider = MockEthProvider::default();

    let policy = TransactionPropagationKind::Trusted;
    let net = Testnet::create_with(2, provider.clone()).await;
    let net = net.with_eth_pool_config_and_policy(Default::default(), policy);

    let handle = net.spawn();

    // connect all the peers
    handle.connect_peers().await;

    let peer_0_handle = &handle.peers()[0];
    let peer_1_handle = &handle.peers()[1];

    let mut peer0_tx_listener = peer_0_handle.pool().unwrap().pending_transactions_listener();
    let mut peer1_tx_listener = peer_1_handle.pool().unwrap().pending_transactions_listener();

    let mut tx_gen = TransactionGenerator::new(rand::rng());
    let tx = tx_gen.gen_eip1559_pooled();

    // ensure the sender has balance
    let sender = tx.sender();
    provider.add_account(sender, ExtendedAccount::new(0, U256::from(100_000_000)));

    // insert the tx in peer0's pool
    let outcome_0 = peer_0_handle.pool().unwrap().add_external_transaction(tx).await.unwrap();
    let inserted = peer0_tx_listener.recv().await.unwrap();

    assert_eq!(inserted, outcome_0.hash);

    // ensure tx is not gossiped to peer1
    peer1_tx_listener.try_recv().expect_err("Empty");

    let mut event_stream_0 = NetworkEventStream::new(peer_0_handle.network().event_listener());
    let mut event_stream_1 = NetworkEventStream::new(peer_1_handle.network().event_listener());

    // disconnect peer1 from peer0
    peer_0_handle.network().remove_peer(*peer_1_handle.peer_id(), PeerKind::Static);
    join!(event_stream_0.next_session_closed(), event_stream_1.next_session_closed());

    // re register peer1 as trusted
    peer_0_handle.network().add_trusted_peer(*peer_1_handle.peer_id(), peer_1_handle.local_addr());
    join!(event_stream_0.next_session_established(), event_stream_1.next_session_established());

    let mut tx_gen = TransactionGenerator::new(rand::rng());
    let tx = tx_gen.gen_eip1559_pooled();

    // ensure the sender has balance
    let sender = tx.sender();
    provider.add_account(sender, ExtendedAccount::new(0, U256::from(100_000_000)));

    // insert pending tx in peer0's pool
    let outcome_1 = peer_0_handle.pool().unwrap().add_external_transaction(tx).await.unwrap();
    let inserted = peer0_tx_listener.recv().await.unwrap();
    assert_eq!(inserted, outcome_1.hash);

    // ensure peer1 now receives the pending txs from peer0
    let mut buff = Vec::with_capacity(2);
    buff.push(peer1_tx_listener.recv().await.unwrap());
    buff.push(peer1_tx_listener.recv().await.unwrap());

    assert!(buff.contains(&outcome_1.hash));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_4844_tx_gossip_penalization() {
    reth_tracing::init_test_tracing();
    let provider = MockEthProvider::default();
    let net = Testnet::create_with(2, provider.clone()).await;

    // install request handlers
    let net = net.with_eth_pool();

    let handle = net.spawn();

    let peer0 = &handle.peers()[0];
    let peer1 = &handle.peers()[1];

    // connect all the peers
    handle.connect_peers().await;

    let mut peer1_tx_listener = peer1.pool().unwrap().pending_transactions_listener();

    let mut tx_gen = TransactionGenerator::new(rand::rng());

    // peer 0 will be penalized for sending txs[0] over gossip
    let txs = vec![tx_gen.gen_eip4844_pooled(), tx_gen.gen_eip1559_pooled()];

    for tx in &txs {
        let sender = tx.sender();
        provider.add_account(sender, ExtendedAccount::new(0, U256::from(100_000_000)));
    }

    let signed_txs: Vec<Arc<TransactionSigned>> =
        txs.iter().map(|tx| Arc::new(tx.transaction().clone().into_inner())).collect();

    let network_handle = peer0.network();

    let peer0_reputation_before =
        peer1.peer_handle().peer_by_id(*peer0.peer_id()).await.unwrap().reputation();

    // sends txs directly to peer1
    network_handle.send_transactions(*peer1.peer_id(), signed_txs);

    let received = peer1_tx_listener.recv().await.unwrap();

    let peer0_reputation_after =
        peer1.peer_handle().peer_by_id(*peer0.peer_id()).await.unwrap().reputation();
    assert_ne!(peer0_reputation_before, peer0_reputation_after);
    assert_eq!(received, *txs[1].transaction().tx_hash());

    // this will return an [`Empty`] error because blob txs are disallowed to be broadcasted
    assert!(peer1_tx_listener.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_sending_invalid_transactions() {
    reth_tracing::init_test_tracing();
    let provider = MockEthProvider::default();
    let net = Testnet::create_with(2, provider.clone()).await;
    // install request handlers
    let net = net.with_eth_pool();

    let handle = net.spawn();

    let peer0 = &handle.peers()[0];
    let peer1 = &handle.peers()[1];

    // connect all the peers
    handle.connect_peers().await;

    assert_eq!(peer0.network().num_connected_peers(), 1);
    let mut peer1_events = peer1.network().event_listener();
    let mut tx_listener = peer1.pool().unwrap().new_transactions_listener();

    for idx in 0..10 {
        // send invalid txs to peer1
        let tx = TxLegacy {
            chain_id: None,
            nonce: idx,
            gas_price: 0,
            gas_limit: 0,
            to: Default::default(),
            value: Default::default(),
            input: Default::default(),
        };
        let tx = TransactionSigned::new_unhashed(tx.into(), Signature::test_signature());
        peer0.network().send_transactions(*peer1.peer_id(), vec![Arc::new(tx)]);
    }

    // await disconnect for bad tx spam
    if let Some(ev) = peer1_events.next().await {
        match ev {
            NetworkEvent::Peer(PeerEvent::SessionClosed { peer_id, .. }) => {
                assert_eq!(peer_id, *peer0.peer_id());
            }
            NetworkEvent::ActivePeerSession { .. } |
            NetworkEvent::Peer(PeerEvent::SessionEstablished { .. }) => {
                panic!("unexpected SessionEstablished event")
            }
            NetworkEvent::Peer(PeerEvent::PeerAdded(_)) => {
                panic!("unexpected PeerAdded event")
            }
            NetworkEvent::Peer(PeerEvent::PeerRemoved(_)) => {
                panic!("unexpected PeerRemoved event")
            }
        }
    }

    // ensure txs never made it to the pool
    assert!(tx_listener.try_recv().is_err());
}
