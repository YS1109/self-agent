use super::*;
use ring::signature::{Ed25519KeyPair, KeyPair};
use rusqlite::{Connection, Transaction};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

struct Database(Mutex<Connection>);
impl UpdateDatabase for Database {
    fn new_transaction<T>(
        &self,
        lifecycle: &crate::reliability::lifecycle_runtime::LifecycleRuntime,
        action: impl FnOnce(&Transaction<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut db = self.0.lock().unwrap();
        lifecycle.check_locked(&db)?;
        let tx = db.transaction()?;
        let result = action(&tx)?;
        tx.commit()?;
        Ok(result)
    }
    fn transaction<T>(
        &self,
        action: impl FnOnce(&Transaction<'_>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut db = self.0.lock().unwrap();
        let tx = db.transaction()?;
        let out = action(&tx)?;
        tx.commit()?;
        Ok(out)
    }
}
struct Fixture {
    lifecycle: Arc<crate::reliability::lifecycle_runtime::LifecycleRuntime>,
    root: PathBuf,
    server: tokio::task::JoinHandle<()>,
    base: String,
    key: Ed25519KeyPair,
    client: reqwest::Client,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
impl Fixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("update-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("stage")).unwrap();
        std::fs::create_dir(root.join("witness")).unwrap();
        let mut db = Connection::open(root.join("state.sqlite")).unwrap();
        let tx = db.transaction().unwrap();
        state::migrate(&tx).unwrap();
        tx.commit().unwrap();
        let lifecycle = crate::reliability::lifecycle_runtime::LifecycleRuntime::new();
        let runtime = UpdateRuntime::new(
            Arc::new(Database(Mutex::new(db))),
            cap_std::fs::Dir::open_ambient_dir(root.join("stage"), cap_std::ambient_authority())
                .unwrap(),
            cap_std::fs::Dir::open_ambient_dir(root.join("witness"), cap_std::ambient_authority())
                .unwrap(),
            "1.0.0".into(),
            4,
            lifecycle.clone(),
        )
        .unwrap();
        // Production mounts inside its existing owner middleware. This fixture
        // proves the routes do not need any public/no-auth exception.
        let app: Router = routes(runtime).layer(axum::middleware::from_fn(
            |request: axum::extract::Request, next: axum::middleware::Next| async move {
                if request
                    .headers()
                    .get(header::AUTHORIZATION)
                    .is_none_or(|v| v != "Bearer fixture-owner")
                {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                next.run(request).await
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let key = Ed25519KeyPair::from_pkcs8(
            Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .unwrap()
                .as_ref(),
        )
        .unwrap();
        Self {
            lifecycle,
            root,
            server,
            base: format!("http://{addr}/api/v1/reliability/updates"),
            key,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
        }
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth("fixture-owner")
    }
    fn trust(&self, expected: Option<u64>, enabled: bool) -> Value {
        let public = self.key.public_key().as_ref();
        json!({"expected_revision":expected,"key_id":"fixture-key","source":"https://updates.example.invalid/stable/","public_key_hex":public.iter().map(|b|format!("{b:02x}")).collect::<String>(),"confirmation_fingerprint":format!("{:x}",Sha256::digest(public)),"enabled":enabled})
    }
    fn begin(&self, id: &str, bytes: &[u8], seq: u64) -> Value {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let manifest=json!({"manifest_schema":1,"application":"self-agent","channel":"stable","source":"https://updates.example.invalid/stable/","key_id":"fixture-key","version":format!("1.0.{seq}"),"sequence":seq,"target":update::Target::current().unwrap(),"database_schema_min":1,"database_schema_max":4,"issued_at":now-1,"expires_at":now+3600,"artifact":{"name":"fixture.bin","bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))}}).to_string();
        let mut signed = b"self-agent-update-manifest-v1\0".to_vec();
        signed.extend_from_slice(manifest.as_bytes());
        json!({"id":id,"config_revision":1,"raw_manifest":manifest,"signature_hex":self.key.sign(&signed).as_ref().iter().map(|b|format!("{b:02x}")).collect::<String>()})
    }
    async fn configure(&self) {
        assert!(self
            .request(reqwest::Method::POST, "/trust")
            .json(&self.trust(None, true))
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    }
}
#[tokio::test]
async fn owner_trust_is_separate_fingerprint_confirmed_and_never_automatic() {
    let f = Fixture::new().await;
    assert_eq!(
        f.client.get(&f.base).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let status: Value = f
        .request(reqwest::Method::GET, "")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["mode"], "verification_disabled");
    assert_eq!(status["automatic_update_enabled"], false);
    let mut input = f.trust(None, true);
    input["confirmation_fingerprint"] = json!("wrong-canary");
    let bad = f
        .request(reqwest::Method::POST, "/trust")
        .json(&input)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert!(!bad.text().await.unwrap().contains("canary"));
    f.configure().await;
    let stale = f
        .request(reqwest::Method::POST, "/trust")
        .json(&f.trust(None, true))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::BAD_REQUEST);
    let status: Value = f
        .request(reqwest::Method::GET, "")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["configuration"]["revision"], 1);
    assert_eq!(status["installed"], false);
}
#[tokio::test]
async fn signed_http_artifact_is_streamed_staged_and_same_id_never_reuploaded() {
    let f = Fixture::new().await;
    f.configure().await;
    let id = uuid::Uuid::new_v4().to_string();
    let bytes = b"this is inert signed fixture content, not an executable";
    let body = f.begin(&id, bytes, 1);
    assert!(f
        .request(reqwest::Method::POST, "/verifications")
        .json(&body)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let path = format!("/verifications/{id}/artifact");
    let uploaded = f
        .request(reqwest::Method::PUT, &path)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert!(
        uploaded.status().is_success(),
        "{}",
        uploaded.text().await.unwrap()
    );
    let receipt: Value = f
        .request(reqwest::Method::GET, &format!("/verifications/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(receipt["receipt"]["status"], "verified");
    assert_eq!(receipt["current_stage_verified"], true);
    assert_eq!(receipt["installed"], false);
    assert_eq!(
        std::fs::read(f.root.join("stage").join(format!(".update-{id}.verified"))).unwrap(),
        bytes
    );
    assert_eq!(
        f.request(reqwest::Method::PUT, &path)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let bad = f
        .request(reqwest::Method::POST, &format!("/stages/{id}/discard"))
        .json(&json!({"expected_sha256":"wrong"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    assert!(f
        .request(reqwest::Method::POST, &format!("/stages/{id}/discard"))
        .json(&json!({"expected_sha256":receipt["receipt"]["metadata"]["sha256"]}))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert_eq!(std::fs::read_dir(f.root.join("stage")).unwrap().count(), 0);
    assert_eq!(
        std::fs::read_dir(f.root.join("witness")).unwrap().count(),
        1
    );
}
#[tokio::test]
async fn unsigned_inputs_unknown_fields_and_inexact_lengths_never_claim_or_echo() {
    let f = Fixture::new().await;
    f.configure().await;
    let id = uuid::Uuid::new_v4().to_string();
    let bytes = b"fixture";
    let mut malicious = f.begin(&id, bytes, 1);
    malicious["trust_key"] = json!("secret-canary");
    let rejected = f
        .request(reqwest::Method::POST, "/verifications")
        .json(&malicious)
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    assert!(!rejected.text().await.unwrap().contains("secret-canary"));
    let mut invalid = f.begin(&id, bytes, 1);
    invalid["signature_hex"] = json!("00".repeat(64));
    assert_eq!(
        f.request(reqwest::Method::POST, "/verifications")
            .json(&invalid)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(f
        .request(reqwest::Method::POST, "/verifications")
        .json(&f.begin(&id, bytes, 1))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let path = format!("/verifications/{id}/artifact");
    assert_eq!(
        f.request(reqwest::Method::PUT, &path)
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request(reqwest::Method::PUT, &path)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(b"wrong-length".to_vec())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let receipt: Value = f
        .request(reqwest::Method::GET, &format!("/verifications/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(receipt["receipt"]["status"], "pending");
    assert!(f
        .request(
            reqwest::Method::POST,
            &format!("/verifications/{id}/cancel")
        )
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert_eq!(std::fs::read_dir(f.root.join("stage")).unwrap().count(), 0);
}
#[test]
fn upload_headers_are_unambiguous_canonical_and_bounded() {
    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    for length in ["0", "01", "-1", "+1", "268435457", "canary"] {
        h.insert(header::CONTENT_LENGTH, length.parse().unwrap());
        assert!(upload_length(&h).is_err());
    }
    h.insert(header::CONTENT_LENGTH, "1".parse().unwrap());
    assert_eq!(upload_length(&h).unwrap(), 1);
    h.append(header::CONTENT_LENGTH, "1".parse().unwrap());
    assert!(upload_length(&h).is_err());
}

#[tokio::test]
async fn draining_update_metadata_returns_static_503_but_validation_remains_400() {
    use crate::reliability::lifecycle_runtime::{
        QuiesceStage, QuiescenceCheck, ShutdownBackend, ShutdownFault,
    };
    struct Backend;
    impl ShutdownBackend for Backend {
        fn stop_external_inputs(&self) {}
        fn quiesce(&self, _: QuiesceStage) -> std::result::Result<QuiescenceCheck, ShutdownFault> {
            Ok(Default::default())
        }
    }
    let f = Fixture::new().await;
    f.lifecycle.request_shutdown(Arc::new(Backend)).unwrap();
    let response = f.request(reqwest::Method::GET, "").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.text().await.unwrap();
    assert!(!body.contains(&f.root.to_string_lossy().to_string()));
    assert!(!body.contains("fixture-owner"));
    let response = f
        .request(reqwest::Method::POST, "/trust")
        .json(&f.trust(None, true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let response = f
        .request(reqwest::Method::POST, "/trust")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
