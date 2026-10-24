//! HTTP API for the Chotu app. Listens on loopback only; `tailscale serve`
//! terminates HTTPS in front of it and is the only way in (see `docs/app-api.md`).

use anyhow::{Context, Result};
use axum::extract::{MatchedPath, Request};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use chotu_common::ApiConfig;
use serde::Serialize;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::{JoinError, JoinHandle};

/// How long shutdown waits for in-flight requests before giving up.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Set by `tailscale serve` to the tailnet login of the caller.
const TAILSCALE_USER_LOGIN: &str = "Tailscale-User-Login";

pub struct ApiServer {
    task: JoinHandle<Result<()>>,
    shutdown: oneshot::Sender<()>,
}

impl ApiServer {
    /// Binds the listener before returning so a bad address or a busy port fails startup.
    pub async fn start(config: &ApiConfig) -> Result<Self> {
        config.validate().map_err(anyhow::Error::msg)?;
        let listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("Failed to bind API server to {}", config.bind))?;
        println!(
            "API server listening on http://{} (expose with `tailscale serve`)",
            config.bind
        );
        Ok(Self::spawn(listener))
    }

    fn spawn(listener: TcpListener) -> Self {
        let (shutdown, shutdown_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(listener, router())
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .context("API server failed")
        });
        Self { task, shutdown }
    }

    /// Resolves when the server task ends on its own.
    pub async fn exited(&mut self) -> Result<Result<()>, JoinError> {
        (&mut self.task).await
    }

    /// Stops accepting connections and waits up to `SHUTDOWN_GRACE` for in-flight requests.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        match tokio::time::timeout(SHUTDOWN_GRACE, self.task).await {
            Ok(Ok(Ok(()))) => println!("API server stopped."),
            Ok(res) => eprintln!("API server stopped with error: {:?}", res),
            Err(_) => eprintln!(
                "API server did not stop within {}s; abandoning in-flight requests.",
                SHUTDOWN_GRACE.as_secs()
            ),
        }
    }
}

fn router() -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .layer(middleware::from_fn(log_request))
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

/// Unauthenticated connection check for the app. Carries no household data.
async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

/// One logfmt line per request. Never logs tokens, bodies, or raw paths.
async fn log_request(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("<unmatched>", MatchedPath::as_str)
        .to_string();
    let tailscale_user = request
        .headers()
        .get(TAILSCALE_USER_LOGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let response = next.run(request).await;

    // member_id stays "-" until device-token auth resolves a member.
    println!(
        "api_request method={} route={} status={} duration_ms={} member_id=- tailscale_user={:?}",
        method,
        route,
        response.status().as_u16(),
        started.elapsed().as_millis(),
        tailscale_user.as_deref().unwrap_or("-"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_non_loopback_bind_before_listening() {
        let config = ApiConfig {
            bind: "0.0.0.0:0".parse().unwrap(),
        };
        let err = ApiServer::start(&config).await.err().expect("must refuse");
        assert!(err.to_string().contains("not a loopback address"), "{err}");
    }

    #[tokio::test]
    async fn serves_health_and_shuts_down_gracefully() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = ApiServer::spawn(listener);

        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/v1/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body, serde_json::json!({ "status": "ok" }));

        let missing = client
            .get(format!("http://{addr}/v1/nope"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);

        tokio::time::timeout(SHUTDOWN_GRACE, server.shutdown())
            .await
            .expect("shutdown finishes");
        assert!(
            client
                .get(format!("http://{addr}/v1/health"))
                .send()
                .await
                .is_err(),
            "listener must be closed after shutdown"
        );
    }
}
