//! Authenticated bounded handshake before bridge effects and every new stream
//! connection. No auto-stop/replay; a responding incompatible peer is not offline.
use harness_core::compatibility::{self, Negotiated, ProtocolRange};
use reqwest::{Client, Url};
use std::time::Duration;
#[derive(Debug)]
pub(crate) enum ProbeError {
    Unavailable,
    Incompatible,
}
impl ProbeError {
    pub(crate) fn permanent(&self) -> bool {
        matches!(self, Self::Incompatible)
    }
}
impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "后台兼容性读取暂未确认；未继续请求",
            Self::Incompatible => ERROR,
        })
    }
}
const ERROR: &str = "后台协议元数据缺失或不兼容；未继续请求，也未停止或替换后台。";
pub(crate) async fn probe(
    client: &Client,
    base: &Url,
    token: &str,
) -> Result<Option<Negotiated>, ProbeError> {
    if base.scheme() != "http"
        || base.host_str() != Some("127.0.0.1")
        || !base.username().is_empty()
        || base.password().is_some()
        || base.path() != "/"
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(ProbeError::Incompatible);
    }
    let mut response = match client
        .get(
            base.join("/api/v1/compatibility")
                .map_err(|_| ProbeError::Incompatible)?,
        )
        .bearer_auth(token)
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) if error.is_connect() && !error.is_timeout() => return Ok(None),
        Err(_) => return Err(ProbeError::Unavailable),
    };
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(ProbeError::Unavailable);
    }
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|n| n > compatibility::MAX_HANDSHAKE_BYTES as u64)
    {
        return Err(ProbeError::Incompatible);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProbeError::Unavailable)?
    {
        if chunk.len() > compatibility::MAX_HANDSHAKE_BYTES.saturating_sub(bytes.len()) {
            return Err(ProbeError::Incompatible);
        }
        bytes.extend_from_slice(&chunk);
    }
    compatibility::negotiate(&bytes, ProtocolRange::local())
        .map(Some)
        .map_err(|_| ProbeError::Incompatible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    async fn fixture(body: Vec<u8>, status: u16) -> (Url, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let work = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0; 1024];
            while !bytes.ends_with(b"\r\n\r\n") {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buf[..n]);
                assert!(bytes.len() < 8192);
            }
            let request = String::from_utf8(bytes).unwrap();
            assert!(request.starts_with("GET /api/v1/compatibility "));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-owner"));
            stream.write_all(format!("HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        });
        (Url::parse(&format!("http://{addr}/")).unwrap(), work)
    }
    #[tokio::test]
    async fn authenticated_fixed_target_handshake_and_responding_old_peer_are_distinct() {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let (url, server) = fixture(
            compatibility::handshake(compatibility::LifecycleState::Running).unwrap(),
            200,
        )
        .await;
        assert_eq!(
            probe(&client, &url, "fixture-owner")
                .await
                .unwrap()
                .unwrap()
                .bounded_history_schema,
            1
        );
        server.await.unwrap();
        for (bytes, status) in [
            (b"secret-canary".to_vec(), 404),
            (vec![b' '; 513], 200),
            (b"{}".to_vec(), 200),
        ] {
            let (url, server) = fixture(bytes, status).await;
            let error = probe(&client, &url, "fixture-owner").await.unwrap_err();
            assert!(!error.to_string().contains("secret-canary"));
            server.await.unwrap();
        }
    }
}
