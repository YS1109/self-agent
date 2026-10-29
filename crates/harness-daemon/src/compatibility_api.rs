//! Mount under existing local-owner bearer authentication. A handshake reports
//! compatibility/lifecycle only, never a durable admission or update authority.
use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use harness_core::compatibility::{self, LifecycleState};
use std::sync::Arc;
type LifecycleReader = Arc<dyn Fn() -> LifecycleState + Send + Sync>;
pub(crate) fn routes<S: Clone + Send + Sync + 'static>(lifecycle: LifecycleReader) -> Router<S> {
    Router::new()
        .route("/api/v1/compatibility", get(handshake))
        .with_state(lifecycle)
}
async fn handshake(State(state): State<LifecycleReader>) -> Response {
    match compatibility::handshake(state()) {
        Ok(bytes) => (
            [
                ("Content-Type", "application/json"),
                ("Cache-Control", "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
fn supported(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all("x-self-agent-protocol").iter();
    match values.next() {
        // Explicit legacy owner HTTP v1 compatibility. This is never an auth
        // bypass and does not claim an old caller negotiated bounded history.
        None => true,
        Some(value) => {
            values.next().is_none()
                && value.to_str().ok().is_some_and(|text| {
                    text.parse::<u16>().ok().is_some_and(|generation| {
                        generation.to_string() == text
                            && (compatibility::PROTOCOL_MIN..=compatibility::PROTOCOL_CURRENT)
                                .contains(&generation)
                    })
                })
        }
    }
}
pub(crate) async fn require_generation(request: Request, next: Next) -> Response {
    if !supported(request.headers()) {
        return (
            StatusCode::UPGRADE_REQUIRED,
            axum::Json(
                serde_json::json!({"error":"客户端协议不兼容；未执行请求，请使用匹配的客户端。"}),
            ),
        )
            .into_response();
    }
    next.run(request).await
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provided_generation_is_exact_unique_and_missing_remains_explicit_legacy() {
        let mut headers = HeaderMap::new();
        assert!(supported(&headers));
        for value in ["0", "2", "01", "1,1", "secret-canary", ""] {
            headers.insert("x-self-agent-protocol", value.parse().unwrap());
            assert!(!supported(&headers));
        }
        headers.insert("x-self-agent-protocol", "1".parse().unwrap());
        assert!(supported(&headers));
        headers.append("x-self-agent-protocol", "1".parse().unwrap());
        assert!(!supported(&headers));
    }

    #[tokio::test]
    async fn generation_rejection_precedes_effects_and_handshake_is_bounded_bare_json() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let writes = Arc::new(AtomicUsize::new(0));
        let counter = writes.clone();
        let app: Router = routes(Arc::new(|| LifecycleState::Running))
            .route(
                "/effect",
                axum::routing::post(move || {
                    let c = counter.clone();
                    async move {
                        c.fetch_add(1, Ordering::AcqRel);
                        StatusCode::OK
                    }
                }),
            )
            .layer(axum::middleware::from_fn(require_generation));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for header in ["0", "2", "01", "canary"] {
            assert_eq!(
                client
                    .post(format!("{base}/effect"))
                    .header("x-self-agent-protocol", header)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::UPGRADE_REQUIRED
            );
        }
        assert_eq!(writes.load(Ordering::Acquire), 0);
        assert!(client
            .post(format!("{base}/effect"))
            .header("x-self-agent-protocol", "1")
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
        let response = client
            .get(format!("{base}/api/v1/compatibility"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.headers()["cache-control"], "no-store");
        let raw = response.bytes().await.unwrap();
        assert!(raw.len() <= 512);
        assert_eq!(
            compatibility::negotiate(&raw, compatibility::ProtocolRange::local())
                .unwrap()
                .protocol,
            1
        );
        server.abort();
    }
}
