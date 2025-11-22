use crate::types::{RpcRequest, RpcResponse, UpstreamConfig};
use crate::upstream::UpstreamNode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time;

//Periodically trigger health check every 10secs
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(10);

pub struct LoadBalancer {
    nodes: Vec<Arc<UpstreamNode>>,
    next_index: AtomicUsize,
}

impl LoadBalancer {
    pub fn new(configs: &[UpstreamConfig]) -> Self {
        let nodes = configs
            .into_iter()
            .map(|config| Arc::new(UpstreamNode::new(config.clone())))
            .collect();

        Self {
            nodes,
            next_index: AtomicUsize::new(0),
        }
    }

    pub fn choose_healthy_node(&self) -> Option<Arc<UpstreamNode>> {
        if self.nodes.is_empty() {
            return None;
        }

        let total_nodes = self.nodes.len();
        let start_index = self.next_index.fetch_add(1, Ordering::SeqCst) % total_nodes;

        for i in 0..total_nodes {
            let index = (start_index + i) % total_nodes;
            let node = &self.nodes[index];

            if node.is_healthy() {
                return Some(Arc::clone(node));
            }
        }

        None
    }

    pub async fn forward_request(&self, request: &RpcRequest) -> Result<RpcResponse, String> {
        let node = self
            .choose_healthy_node()
            .ok_or_else(|| "No healthy nodes available".to_string())?;

        node.call_rpc(request).await
    }

    pub fn start_health_checker(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = time::interval(HEALTH_CHECK_INTERVAL);

            loop {
                interval.tick().await;

                for node in &self.nodes {
                    let node = Arc::clone(node);
                    tokio::spawn(async move {
                        let is_healthy = node.check_health().await;
                        let status = if is_healthy { "HEALTHY" } else { "UNHEALTHY" };
                        println!("Health check result for {}: {}", node.get_name(), status);
                    });
                }
            }
        });
    }

    pub fn get_nodes_status(&self) -> Vec<(String, String)> {
        self.nodes
            .iter()
            .map(|node| {
                let status = match node.get_status() {
                    crate::upstream::NodeCondition::Healthy => "HEALTHY",
                    crate::upstream::NodeCondition::Unhealthy => "UNHEALTHY",
                };
                (node.get_name().to_string(), status.to_string())
            })
            .collect()
    }
}
