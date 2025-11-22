//! Upstream RPC node management with circuit breaker pattern.
//!
//! This module provides the core functionality for managing upstream RPC nodes,
//! including health monitoring, circuit breaking, and request routing. Each node
//! tracks its own health status and automatically opens the circuit after consecutive
//! failures to prevent cascading failures.
//!
//! # Circuit Breaker Pattern
//!
//! The circuit breaker has three states:
//! - **Healthy**: Node is operational and accepting requests
//! - **Unhealthy**: Node has failed too many times and is temporarily disabled
//! - **Cooldown**: After a cooldown period, unhealthy nodes can be retried
use crate::types::{RpcRequest, RpcResponse, UpstreamConfig};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Maximum number of consecutive failures before opening the circuit.
///
/// When a node reaches this threshold, it transitions to `NodeCondition::Unhealthy`
/// and will not receive requests until the cooldown period expires.
const MAX_CONSECUTIVE_FAILURES: usize = 3;

/// Duration a node must wait in unhealthy state before attempting recovery.
///
/// After this cooldown period, the node becomes eligible for health checks
/// and can potentially transition back to healthy state.
const COOLDOWN_DURATION: Duration = Duration::from_secs(60);

/// Timeout duration for individual RPC requests.
///
/// Requests exceeding this duration will be considered failed and contribute
/// to the circuit breaker's failure count.
const REQ_TIMEOUT: Duration = Duration::from_secs(5);

/// Health status of an upstream RPC node.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NodeCondition {
    /// Node is operational and accepting requests.
    ///
    /// In this state, the node will receive traffic according to the load balancing
    /// strategy. Successful requests keep the node healthy, while failures increment
    /// the consecutive failure counter.
    Healthy,

    /// Node has exceeded the failure threshold and is temporarily disabled.
    ///
    /// In this state, the node will not receive any traffic until the cooldown
    /// period expires. After cooldown, a health check can transition the node
    /// back to healthy state.
    Unhealthy,
}

/// Represents a single upstream RPC node with circuit breaker logic.
///
/// Each `UpstreamNode` encapsulates:
/// - Configuration (name, URL)
/// - Health status tracking with thread-safe state management
/// - Consecutive failure counting
/// - HTTP client for making RPC requests
///
/// # Circuit Breaker Behavior
///
/// - After `MAX_CONSECUTIVE_FAILURES` failures, the node transitions to unhealthy
/// - Unhealthy nodes enter a cooldown period of `COOLDOWN_DURATION`
/// - Successful requests reset the failure counter and restore health
pub struct UpstreamNode {
    /// Configuration containing node name and URL.
    pub config: UpstreamConfig,

    /// Current health status and failure timestamp
    status: RwLock<NodeState>,

    /// Count of consecutive failures
    consecutive_failures: AtomicUsize,

    /// HTTP client configured with timeout for making RPC requests.
    client: reqwest::Client,
}

/// Internal state of a node state
#[derive(Debug, Clone)]
struct NodeState {
    /// Current health condition of the node.
    health_status: NodeCondition,

    /// Timestamp of the last failure, used to calculate cooldown expiration.
    /// `None` indicates the node has never failed or has fully recovered.
    last_failure_time: Option<Instant>,
}

impl UpstreamNode {
    /// Creates a new upstream node with the given configuration.
    ///
    /// The node is initialized in a healthy state with zero failures.
    /// An HTTP client is created with the configured timeout.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration containing the node's name and URL
    /// 
    pub fn new(config: UpstreamConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(REQ_TIMEOUT)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            config,
            status: RwLock::new(NodeState {
                health_status: NodeCondition::Healthy,
                last_failure_time: None,
            }),
            consecutive_failures: AtomicUsize::new(0),
            client,
        }
    }

    /// Checks if the node is currently healthy and ready to accept requests.
    ///
    /// A node is considered healthy if:
    /// - Its status is `NodeCondition::Healthy`, OR
    /// - Its status is `NodeCondition::Unhealthy` but the cooldown period has expired
    ///
    pub fn is_healthy(&self) -> bool {
        let state = self.status.read();
        match state.health_status {
            NodeCondition::Healthy => true,
            NodeCondition::Unhealthy => {
                // Check if cooldown period has expired
                if let Some(last_failure) = state.last_failure_time {
                    if Instant::now().duration_since(last_failure) >= COOLDOWN_DURATION {
                        tracing::info!(
                            "Node {} cooldown period expired, allowing retry",
                            self.config.name
                        );
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Performs an active health check by calling `eth_blockNumber`.
    pub async fn check_health(&self) -> bool {
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "eth_blockNumber".to_string(),
            params: serde_json::Value::Array(vec![]),
            id: serde_json::Value::String("health_check".to_string()),
        };

        match self.call_rpc_internal(&request).await {
            Ok(_) => {
                self.record_success();
                true
            }
            Err(e) => {
                tracing::warn!("Health check failed for node {}: {}", self.config.name, e);
                self.record_failure();
                false
            }
        }
    }

    /// Calls the upstream RPC node with the given request.
    pub async fn call_rpc(&self, request: &RpcRequest) -> Result<RpcResponse, String> {
        self.call_rpc_internal(request).await.map_err(|e| {
            self.record_failure();
            e
        })
    }

    async fn call_rpc_internal(&self, request: &RpcRequest) -> Result<RpcResponse, String> {
        let response = self
            .client
            .post(&self.config.url)
            .json(request)
            .send()
            .await
            .map_err(|e| format!("Request failed: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("HTTP error: {}", response.status()));
        }

        let rpc_response: RpcResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse response: {}", e))?;

        if rpc_response.error.is_some() {
            return Err(format!("RPC error: {:?}", rpc_response.error));
        }

        self.record_success();
        Ok(rpc_response)
    }

    
    /// Records a successful request and potentially recovers the node.
    ///
    /// This method:
    /// - Resets the consecutive failure counter to zero
    /// - Transitions unhealthy nodes back to healthy state
    /// - Clears the last failure timestamp
    fn record_success(&self) {
        let prev_failures = self.consecutive_failures.swap(0, Ordering::SeqCst);
        let mut state = self.status.write();
        if state.health_status == NodeCondition::Unhealthy {
            tracing::info!("Node {} recovered and marked HEALTHY", self.config.name);
            state.health_status = NodeCondition::Healthy;
            state.last_failure_time = None;
        } else if prev_failures > 0 {
            tracing::debug!(
                "Node {} success, reset failure count from {}",
                self.config.name,
                prev_failures
            );
        }
    }

    /// Records a failed request and potentially opens the circuit.
    ///
    /// This method:
    /// - Increments the consecutive failure counter atomically
    /// - Transitions to unhealthy state after reaching the threshold
    /// - Records the failure timestamp for cooldown tracking
    fn record_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::warn!("Node {} failure #{} recorded", self.config.name, failures);
        if failures >= MAX_CONSECUTIVE_FAILURES {
            let mut state = self.status.write();
            if state.health_status == NodeCondition::Healthy {
                tracing::error!(
                    "Node {} reached {} consecutive failures, marking UNHEALTHY",
                    self.config.name,
                    failures
                );
                state.health_status = NodeCondition::Unhealthy;
                state.last_failure_time = Some(Instant::now());
            }
        }
    }

    pub fn get_name(&self) -> &str {
        &self.config.name
    }

    /// Returns the current health status of this node.
    ///
    /// # Returns
    ///
    /// The current `NodeCondition` (Healthy or Unhealthy)
    pub fn get_status(&self) -> NodeCondition {
        self.status.read().health_status
    }

    // Test helper
    #[cfg(test)]
    pub fn get_consecutive_failures(&self) -> usize {
        self.consecutive_failures.load(Ordering::SeqCst)
    }

    /// Test helper, allows testing circuit breaker logic.
    #[cfg(test)]
    pub fn force_mark_failure(&self) {
        self.record_failure();
    }

    /// Test helper, allows testing recovery logic.
    #[cfg(test)]
    pub fn force_mark_success(&self) {
        self.record_success();
    }
}




#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_node(name: &str) -> UpstreamNode {
        UpstreamNode::new(UpstreamConfig {
            name: name.to_string(),
            url: "http://invalid-test-url:9999".to_string(),
        })
    }


    #[test]
    fn test_single_failure_keeps_node_healthy() {
        let node = create_test_node("TestNode");
        node.force_mark_failure();
        assert_eq!(node.get_status(), NodeCondition::Healthy);
        assert!(node.is_healthy());
        assert_eq!(node.get_consecutive_failures(), 1);
    }

    #[test]
    fn test_three_failures_opens_circuit() {
        let node = create_test_node("TestNode");

        // Balancer shall break the circuit here.
        node.force_mark_failure();
        node.force_mark_failure();
        node.force_mark_failure();
        
        assert_eq!(node.get_status(), NodeCondition::Unhealthy);
        assert!(!node.is_healthy());
        assert_eq!(node.get_consecutive_failures(), 3);
    }

    #[test]
    fn test_success_resets_failure_count() {
        let node = create_test_node("TestNode");
        
        // Two failures
        node.force_mark_failure();
        node.force_mark_failure();
        assert_eq!(node.get_consecutive_failures(), 2);
        
        // Resets the counter
        node.force_mark_success();
        
        assert_eq!(node.get_status(), NodeCondition::Healthy);
        assert!(node.is_healthy());
        assert_eq!(node.get_consecutive_failures(), 0);
    }

    #[test]
    fn test_circuit_recovery_after_success() {
        let node = create_test_node("TestNode");
        
        // Open the circuit
        node.force_mark_failure();
        node.force_mark_failure();
        node.force_mark_failure();
        assert_eq!(node.get_status(), NodeCondition::Unhealthy);
        
        // Success should close the circuit
        node.force_mark_success();
        
        assert_eq!(node.get_status(), NodeCondition::Healthy);
        assert!(node.is_healthy());
        assert_eq!(node.get_consecutive_failures(), 0);
    }

    #[test]
    fn test_concurrent_failure_tracking() {
        use std::sync::Arc;
        use std::thread;
        
        let node = Arc::new(create_test_node("TestNode"));
        let mut handles = vec![];
        
        // Simulate concurrent failures from multiple threads
        for _ in 0..10 {
            let node_clone = Arc::clone(&node);
            let handle = thread::spawn(move || {
                node_clone.force_mark_failure();
            });
            handles.push(handle);
        }
        
        for handle in handles {
            handle.join().unwrap();
        }
        
        // Should have exactly 10 failures
        assert_eq!(node.get_consecutive_failures(), 10);
        assert_eq!(node.get_status(), NodeCondition::Unhealthy);
    }
}
