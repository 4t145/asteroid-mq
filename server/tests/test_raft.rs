use asteroid_mq::protocol::node::{Node, NodeConfig, NodeId};
use std::{
    net::{Ipv4Addr, SocketAddr},
    time::Duration,
};
mod common;

#[tokio::test(flavor = "multi_thread")]
async fn test_raft() {
    // let console_layer = console_subscriber::spawn();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    fn raft_config() -> openraft::Config {
        openraft::Config {
            cluster_name: "test".to_string(),
            heartbeat_interval: 200,
            election_timeout_max: 1000,
            election_timeout_min: 500,
            ..Default::default()
        }
    }
    const fn node_id(index: usize) -> NodeId {
        NodeId::new_indexed(index as u64)
    }
    const fn node_addr(index: usize) -> SocketAddr {
        SocketAddr::new(
            std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
            19000 + index as u16,
        )
    }
    let cluster = common::TestClusterProvider::new(map!(
        node_id(2) => node_addr(2),
    ));

    let node_1 = Node::new(NodeConfig {
        id: node_id(1),
        addr: node_addr(1),
        raft: raft_config(),
        ..Default::default()
    });

    let node_2 = Node::new(NodeConfig {
        id: node_id(2),
        addr: node_addr(2),
        raft: raft_config(),
        ..Default::default()
    });

    let node_3 = Node::new(NodeConfig {
        id: node_id(3),
        addr: node_addr(3),
        raft: raft_config(),
        ..Default::default()
    });

    node_2.init_raft(cluster.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    cluster
        .update(map!(
            node_id(1) => node_addr(1),
            node_id(2) => node_addr(2),
        ))
        .await;
    node_1.init_raft(cluster.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    node_1
        .raft()
        .await
        .with_raft_state(|rs| {
            tracing::info!(?rs.server_state);
        })
        .await
        .unwrap();

    cluster
        .update(map!(
            node_id(1) => node_addr(1),
            node_id(2) => node_addr(2),
            node_id(3) => node_addr(3),
        ))
        .await;
    node_3.init_raft(cluster.clone()).await.unwrap();
    node_3
        .raft()
        .await
        .with_raft_state(|f| {
            tracing::info!(?f.membership_state);
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    drop(node_2);
    cluster
        .update(map!(
            node_id(1) => node_addr(1),
            node_id(3) => node_addr(3),
        ))
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    node_1.raft().await.trigger().heartbeat().await.unwrap();
    node_3.raft().await.trigger().heartbeat().await.unwrap();
    cluster
        .update(map!(
            node_id(1) => node_addr(1),
            node_id(3) => node_addr(3),
        ))
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let result_1 = node_1
        .raft()
        .await
        .with_raft_state(|s| {
            tracing::info!("node_1 state: {:#?}", s.membership_state);
        })
        .await;
    tracing::info!(
        "node_1 leader: {:#?}",
        node_1.raft().await.current_leader().await
    );
    let result_3 = node_3
        .raft()
        .await
        .with_raft_state(|s| {
            tracing::info!("node_3 state: {:#?}", s.membership_state);
        })
        .await;
    tracing::info!(
        "node_3 state: {:#?}",
        node_3.raft().await.current_leader().await
    );
    result_1.unwrap();
    result_3.unwrap();

    tokio::time::sleep(Duration::from_secs(10)).await;
}
