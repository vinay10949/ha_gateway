use crate::types::{RpcRequest, RpcResponse, UpstreamConfig};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const MAX_CONSECUTIVE_FAILURES: usize = 3;
const COOLDOWN_DURATION: Duration = Duration::from_secs(60);
const REQ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NodeCondition {
    Healthy,
    Unhealthy,
}

/// Represents a single upstream RPC node with circuit breaker logic
pub struct UpstreamNode {
    pub config: UpstreamConfig,
    status: RwLock<NodeState>,
    consecutive_failures: AtomicUsize,
    client: reqwest::Client,
}

#[derive(Debug, Clone)]
struct NodeState {
    health_status: NodeCondition,
    last_failure_time: Option<Instant>,
}

impl UpstreamNode {
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

    /// Get current health status
    pub fn is_healthy(&self) -> bool {
        let state = self.status.read();
        match state.health_status {
            NodeCondition::Healthy => true,
            NodeCondition::Unhealthy => {
                if let Some(last_failure) = state.last_failure_time {
                    if Instant::now().duration_since(last_failure) >= COOLDOWN_DURATION {
                        return false;
                    }
                }
                return false;
            }
        }
    }

    /// Perform a health check by calling eth_blockNumber
    pub async fn check_health(&self) -> bool {
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "eth_blockNumber".to_string(),
            params: serde_json::Value::Array(vec![]),
            id: serde_json::Value::String("health_check".to_string()),
        };

        match self.call_rpc_internal(&request).await {
            Ok(_) => {
                self.mark_success();
                true
            }
            Err(e) => {
                self.mark_failure();
                false
            }
        }
    }

    /// Call the upstream RPC node
    pub async fn call_rpc(&self, request: &RpcRequest) -> Result<RpcResponse, String> {
        self.call_rpc_internal(request).await.map_err(|e| {
            self.mark_failure();
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

        self.mark_success();
        Ok(rpc_response)
    }

    fn mark_success(&self) {
        self.consecutive_failures.swap(0, Ordering::SeqCst);
        let mut state = self.status.write();
        if state.health_status == NodeCondition::Unhealthy {
            state.health_status = NodeCondition::Healthy;
            state.last_failure_time = None;
        }
    }

    fn mark_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::SeqCst) + 1;

        if failures >= MAX_CONSECUTIVE_FAILURES {
            let mut state = self.status.write();
            if state.health_status == NodeCondition::Healthy {
                state.health_status = NodeCondition::Unhealthy;
                state.last_failure_time = Some(Instant::now());
            }
        }
    }

    pub fn get_name(&self) -> &str {
        &self.config.name
    }

    pub fn get_status(&self) -> NodeCondition {
        self.status.read().health_status
    }
}
