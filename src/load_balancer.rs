//! Load balancer with round-robin selection and automatic health checking.
//!
//! This module implements a load balancer that distributes RPC requests across
//! multiple upstream nodes using a round-robin strategy, while automatically
//! monitoring node health and avoiding unhealthy nodes.
//!
//! # Load Balancing Strategy
//!
//! The load balancer uses **round-robin with health filtering**:
//! 1. Maintains a rotating index across all nodes
//! 2. Skips unhealthy nodes during selection
//! 3. Distributes load evenly across healthy nodes
//!
//! # Health Monitoring
//!
//! A background task periodically checks the health of all nodes:
//! - Runs every 10 seconds
//! - Executes health checks concurrently for all nodes
//! - Updates node status based on check results

use crate::types::{RpcRequest, RpcResponse, UpstreamConfig};
use crate::upstream::UpstreamNode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time;

/// Interval between health check cycles.
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(10);

/// Load balancer for distributing requests across multiple upstream RPC nodes.
pub struct LoadBalancer {
    /// List of upstream nodes wrapped in Arc for shared ownership.
    nodes: Vec<Arc<UpstreamNode>>,

    /// Atomic counter for round-robin node selection.
    next_index: AtomicUsize,
}

impl LoadBalancer {
    /// Initalizes a new load balancer with the given upstream node configurations.
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

    /// Selects a healthy node using round-robin strategy.
    ///
    /// This method iterates through all nodes starting from the current round-robin
    /// index, returning the first healthy node found. The index is incremented
    /// atomically to ensure fair distribution across concurrent requests.
    pub fn choose_healthy_node(&self) -> Option<Arc<UpstreamNode>> {
        if self.nodes.is_empty() {
            tracing::error!("No Upstream Nodes registered.");
            return None;
        }

        let total_nodes = self.nodes.len();
        let start_index = self.next_index.fetch_add(1, Ordering::SeqCst) % total_nodes;

        for i in 0..total_nodes {
            let index = (start_index + i) % total_nodes;
            let node = &self.nodes[index];

            if node.is_healthy() {
                tracing::debug!("Selected healthy node: {}", node.get_name());
                return Some(Arc::clone(node));
            }
        }

        tracing::error!("No healthy nodes available!");
        None
    }

    /// Forwards an RPC request to a healthy upstream node.
    ///
    /// This is the main entry point for request routing. It selects a healthy
    /// node and forwards the request to it.
    /// ```
    pub async fn forward_request(&self, request: &RpcRequest) -> Result<RpcResponse, String> {
        let node = self
            .choose_healthy_node()
            .ok_or_else(|| "No healthy nodes available".to_string())?;
        tracing::info!("Forwarding request to Node {}", node.get_name());
        node.call_rpc(request).await
    }

    /// Starts a background task that periodically checks the health of all nodes.
    ///
    /// It runs indefinitely, performing health checks on all nodes at regular intervals. 
    /// Each node's health check runs concurrently in its own task.
    ///
    /// # Behavior
    ///
    /// - Runs every `HEALTH_CHECK_INTERVAL` (10 seconds)
    /// - Spawns a separate task for each node's health check
    /// - Logs the health status of each node
    /// - Continues running until the program terminates
    pub fn start_health_checker(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut interval = time::interval(HEALTH_CHECK_INTERVAL);
            tracing::info!("Running health checks on all nodes...");

            loop {
                interval.tick().await;

                for node in &self.nodes {
                    let node = Arc::clone(node);
                    tokio::spawn(async move {
                        let is_healthy = node.check_health().await;
                        let status = if is_healthy { "HEALTHY" } else { "UNHEALTHY" };
                        tracing::info!("Health check status for {}: {}", node.get_name(), status);
                    });
                }
            }
        });
    }

    /// Returns the current health status of all nodes.
    ///
    /// This method provides a snapshot of the health status of all registered
    /// nodes, useful for monitoring and debugging.
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
