mod cache;
mod load_balancer;
mod types;
mod upstream;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use cache::Cache;
use load_balancer::LoadBalancer;
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use types::{RpcRequest, RpcResponse, UpstreamConfig};

#[derive(Clone)]
struct AppState {
    load_balancer: Arc<LoadBalancer>,
    cache: Arc<Cache>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ha_gateway=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Starting HA Gateway");

    // Using local proxies to eth nodes.
    let upstreams = vec![
        UpstreamConfig {
            name: "Node 1".to_string(),
            url: "http://localhost:8545".to_string(),
        },
        UpstreamConfig {
            name: "Node 2".to_string(),
            url: "http://localhost:8546".to_string(),
        },
        UpstreamConfig {
            name: "Node 3".to_string(),
            url: "http://localhost:8547".to_string(),
        },
    ];

    tracing::info!("Configured {} upstream nodes", upstreams.len());
    for upstream in &upstreams {
        tracing::info!("  - {}: {}", upstream.name, upstream.url);
    }

    // Create load balancer and start health checker
    let load_balancer = Arc::new(LoadBalancer::new(&upstreams));
    let cache = Arc::new(Cache::new());

    // Start background health checker
    Arc::clone(&load_balancer).start_health_checker();

    let state = AppState {
        load_balancer: Arc::clone(&load_balancer),
        cache,
    };

    // Build router
    let app = Router::new()
        .route("/", post(handle_rpc_request))
        .route("/health", get(health_check))
        .route("/status", get(status_check))
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http());

    // Start server
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080")
        .await
        .expect("Failed to bind to port 8080");

    tracing::info!("Listening on http://0.0.0.0:8080");

    axum::serve(listener, app)
        .await
        .expect("Server failed to start");
}

async fn handle_rpc_request(
    State(state): State<AppState>,
    Json(request): Json<RpcRequest>,
) -> impl IntoResponse {
    tracing::info!("Received RPC request: method={}", request.method);

    let cache_key = if request.method == "eth_blockNumber" {
        Some(format!(
            "{}:{}",
            request.method,
            serde_json::to_string(&request.params).unwrap_or_default()
        ))
    } else {
        None
    };

    if let Some(ref key) = cache_key {
        tracing::info!("checking key in cache {:?}",cache_key);
        if let Some(cached_result) = state.cache.get(key) {
            tracing::info!("Received cache result  {:?}",cached_result);
            return (
                StatusCode::OK,
                Json(RpcResponse::success(request.id.clone(), cached_result)),
            );
        }
    }

    // Forward to upstream
    match state.load_balancer.forward_request(&request).await {
        Ok(response) => {
            // Cache successful responses for cacheable methods
            if let (Some(key), Some(result)) = (cache_key, &response.result) {
                state.cache.put(key, result.clone());
            }

            tracing::info!("Successfully forwarded request");
            (StatusCode::OK, Json(response))
        }
        Err(e) => {
            tracing::error!("Failed to forward request: {}", e);
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(RpcResponse::error(
                    request.id.clone(),
                    -32603,
                    format!("Internal error: {}", e),
                )),
            )
        }
    }
}

/// Health check endpoint
async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

/// Status check endpoint - returns status of all upstream nodes
async fn status_check(State(state): State<AppState>) -> impl IntoResponse {
    let nodes_status = state.load_balancer.get_nodes_status();
    let status_json = serde_json::json!({
        "nodes": nodes_status.iter().map(|(name, status)| {
            serde_json::json!({
                "name": name,
                "status": status
            })
        }).collect::<Vec<_>>()
    });

    (StatusCode::OK, Json(status_json))
}
