//! Local-owner verifier/stager routes. Mount INSIDE the daemon owner middleware;
//! never expose these through gateway/QQ. This module does not fetch or execute.
use super::{
    update,
    update_runtime::{RuntimeReceipt, RuntimeStatus, UpdateDatabase, UpdateRuntime},
    update_state as state,
};
use axum::{
    body::{Body, BodyDataStream, Bytes},
    extract::{rejection::JsonRejection, DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use futures_util::Stream;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    pin::Pin,
    sync::{atomic::AtomicBool, Arc},
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

#[derive(Debug)]
struct Invalid(StatusCode);
impl IntoResponse for Invalid {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error":"更新验证操作未确认；请只读查询原记录。没有下载、执行、安装或自动重发。"}))).into_response()
    }
}
fn invalid(_: impl std::fmt::Display) -> Invalid {
    Invalid(StatusCode::BAD_REQUEST)
}
type Result<T> = std::result::Result<T, Invalid>;

pub(crate) fn routes<D: UpdateDatabase, S: Clone + Send + Sync + 'static>(
    runtime: Arc<UpdateRuntime<D>>,
) -> Router<S> {
    Router::new()
        .route("/api/v1/reliability/updates", get(status::<D>))
        .route("/api/v1/reliability/updates/trust", post(configure::<D>))
        .route(
            "/api/v1/reliability/updates/verifications",
            post(begin::<D>),
        )
        .route(
            "/api/v1/reliability/updates/verifications/{id}",
            get(receipt::<D>),
        )
        .route(
            "/api/v1/reliability/updates/verifications/{id}/artifact",
            put(upload::<D>),
        )
        .route(
            "/api/v1/reliability/updates/verifications/{id}/cancel",
            post(cancel::<D>),
        )
        .route(
            "/api/v1/reliability/updates/stages/{id}/discard",
            post(discard::<D>),
        )
        .layer(DefaultBodyLimit::max(128 * 1024))
        .with_state(runtime)
}
async fn status<D: UpdateDatabase>(
    State(r): State<Arc<UpdateRuntime<D>>>,
) -> Result<Json<RuntimeStatus>> {
    r.status().await.map(Json).map_err(invalid)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustRequest {
    expected_revision: Option<u64>,
    public_key_hex: String,
    key_id: String,
    source: String,
    enabled: bool,
    confirmation_fingerprint: String,
}
fn decode_key(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("key"));
    }
    let mut key = [0; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(invalid)?;
    }
    Ok(key)
}
fn uuid(value: &str) -> Result<()> {
    if uuid::Uuid::parse_str(value).is_ok_and(|v| v.to_string() == value) {
        Ok(())
    } else {
        Err(invalid("id"))
    }
}
async fn configure<D: UpdateDatabase>(
    State(r): State<Arc<UpdateRuntime<D>>>,
    body: std::result::Result<Json<TrustRequest>, JsonRejection>,
) -> Result<Json<state::TrustConfig>> {
    let Json(b) = body.map_err(invalid)?;
    let public_key = decode_key(&b.public_key_hex)?;
    // This confirmation is outside the later verify request. A manifest cannot
    // nominate its own trust root or source, even when its signature is valid.
    if b.confirmation_fingerprint != format!("{:x}", Sha256::digest(public_key)) {
        return Err(invalid("confirmation"));
    }
    r.configure(
        b.expected_revision,
        state::TrustInput {
            key_id: b.key_id,
            source: b.source,
            public_key,
            enabled: b.enabled,
        },
    )
    .await
    .map(Json)
    .map_err(invalid)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginRequest {
    id: String,
    config_revision: u64,
    raw_manifest: String,
    signature_hex: String,
}
async fn begin<D: UpdateDatabase>(
    State(r): State<Arc<UpdateRuntime<D>>>,
    body: std::result::Result<Json<BeginRequest>, JsonRejection>,
) -> Result<Json<state::Receipt>> {
    let Json(b) = body.map_err(invalid)?;
    uuid(&b.id)?;
    r.begin(
        b.id,
        b.config_revision,
        state::ManifestEvidence {
            raw_manifest: b.raw_manifest,
            signature_hex: b.signature_hex,
        },
    )
    .await
    .map(Json)
    .map_err(invalid)
}
async fn receipt<D: UpdateDatabase>(
    State(r): State<Arc<UpdateRuntime<D>>>,
    Path(id): Path<String>,
) -> Result<Json<RuntimeReceipt>> {
    uuid(&id)?;
    r.receipt(id)
        .await
        .map_err(invalid)?
        .map(Json)
        .ok_or(Invalid(StatusCode::NOT_FOUND))
}
async fn cancel<D: UpdateDatabase>(
    State(r): State<Arc<UpdateRuntime<D>>>,
    Path(id): Path<String>,
) -> Result<Json<state::Receipt>> {
    uuid(&id)?;
    r.cancel(id).await.map(Json).map_err(invalid)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscardRequest {
    expected_sha256: String,
}
async fn discard<D: UpdateDatabase>(
    State(r): State<Arc<UpdateRuntime<D>>>,
    Path(id): Path<String>,
    body: std::result::Result<Json<DiscardRequest>, JsonRejection>,
) -> Result<Json<state::Receipt>> {
    uuid(&id)?;
    let Json(b) = body.map_err(invalid)?;
    r.discard(id, b.expected_sha256)
        .await
        .map(Json)
        .map_err(invalid)
}
/// Pulls at most the verifier's requested chunk; never buffers the whole upload.
/// The signed byte count is rechecked by the staging primitive including EOF.
struct BodyReader {
    stream: BodyDataStream,
    chunk: Bytes,
}
impl AsyncRead for BodyReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if !self.chunk.is_empty() {
                let size = self.chunk.len().min(output.remaining());
                output.put_slice(&self.chunk.split_to(size));
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut self.stream).poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => self.chunk = chunk,
                Poll::Ready(Some(Err(_))) => {
                    return Poll::Ready(Err(std::io::Error::other("upload_read_failed")))
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
fn upload_length(headers: &HeaderMap) -> Result<u64> {
    let mut lengths = headers.get_all(header::CONTENT_LENGTH).iter();
    let raw = lengths.next().ok_or_else(|| invalid("length required"))?;
    if lengths.next().is_some() || headers.contains_key(header::TRANSFER_ENCODING) {
        return Err(invalid("ambiguous length"));
    }
    let raw = raw.to_str().map_err(invalid)?;
    let size: u64 = raw.parse().map_err(invalid)?;
    if raw != size.to_string() || size == 0 || size > update::MAX_ARTIFACT_BYTES {
        return Err(invalid("length"));
    }
    let mut types = headers.get_all(header::CONTENT_TYPE).iter();
    if types.next().is_none_or(|v| v != "application/octet-stream") || types.next().is_some() {
        return Err(invalid("content type"));
    }
    Ok(size)
}
async fn upload<D: UpdateDatabase>(
    State(r): State<Arc<UpdateRuntime<D>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<state::Receipt>> {
    uuid(&id)?;
    let size = upload_length(&headers)?;
    let original = r
        .receipt(id.clone())
        .await
        .map_err(invalid)?
        .ok_or(Invalid(StatusCode::NOT_FOUND))?;
    if original.receipt.metadata.bytes != size || original.receipt.status != state::Status::Pending
    {
        return Err(invalid("metadata"));
    }
    r.upload(
        id,
        BodyReader {
            stream: body.into_data_stream(),
            chunk: Bytes::new(),
        },
        Arc::new(AtomicBool::new(false)),
    )
    .await
    .map(Json)
    .map_err(invalid)
}

#[cfg(test)]
#[path = "update_api_tests.rs"]
mod tests;
