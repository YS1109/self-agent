use anyhow::{bail, Context, Result};
use axum::{extract::State, Json};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use crate::{
    lifecycle_error,
    reliability::lifecycle::{AdmissionError, WorkerExit, WorkerKind},
    ApiError, ApiResult, AppState,
};

#[derive(Default, Deserialize)]
pub(crate) struct ModelRequest {
    base_url: Option<String>,
    models_endpoint: Option<String>,
    api_key: Option<String>,
    executor: Option<String>,
    provider_source: Option<String>,
    provider_id: Option<String>,
}

pub(crate) fn service_url(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim()).context("模型服务地址无效")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("模型服务地址必须使用 HTTP 或 HTTPS，不能包含凭证、查询参数或片段");
    }
    Ok(url)
}

pub(crate) fn models_url(base_url: &str, endpoint: &str) -> Result<Url> {
    let base = service_url(base_url)?;
    let endpoint = endpoint.trim();
    if endpoint.len() > 2048 || endpoint.chars().any(char::is_control) {
        bail!("模型列表端点不能超过 2048 字节或包含控制字符");
    }
    let directory = Url::parse(&format!("{}/", base.as_str().trim_end_matches('/')))
        .context("模型服务地址无效")?;
    let url = if endpoint.is_empty() {
        directory.join("models")
    } else {
        directory.join(endpoint)
    }
    .context("模型列表端点无效")?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("模型列表端点必须使用 HTTP 或 HTTPS，不能包含凭证或片段");
    }
    if url.origin() != base.origin() {
        bail!("模型列表端点必须与模型服务同源，请使用相同协议、主机和端口");
    }
    Ok(url)
}

pub(crate) fn normalize_models(rows: &[Value], codex: bool) -> Result<Vec<Value>> {
    if rows.len() > 2000 {
        bail!("模型列表超过 2000 项限制");
    }
    let mut models = BTreeMap::new();
    for row in rows {
        if codex && row["hidden"].as_bool() == Some(true) {
            continue;
        }
        let id = if codex { &row["model"] } else { &row["id"] };
        let Some(id) = id
            .as_str()
            .map(str::trim)
            .filter(|id| !id.is_empty() && crate::configuration::validate_model(id).is_ok())
        else {
            continue;
        };
        let name = row["displayName"]
            .as_str()
            .filter(|name| name.len() <= 200 && !name.chars().any(char::is_control))
            .unwrap_or(id);
        models.insert(id.to_owned(), json!({"id":id, "name":name}));
    }
    Ok(models.into_values().collect())
}

pub(crate) async fn discover(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ModelRequest>,
) -> ApiResult<Json<Value>> {
    let (admission, ()) = state.admit_store(|_| Ok(()))?;
    let receiver = admission
        .spawn(WorkerKind::Probe, async move {
            let stop = state.lifecycle.signal();
            let process = request.executor.as_deref() == Some("codex");
            let request = discover_owned(state, request);
            let result = if process {
                // Codex owns and awaits process cleanup after cancellation; never
                // select-drop a future which still owns the original Child.
                request.await
            } else {
                tokio::select! {
                    _ = stop.cancelled() => Err(anyhow::anyhow!(AdmissionError::Draining)),
                    result = request => result,
                }
            };
            let exit = if result
                .as_ref()
                .err()
                .is_some_and(|error| error.is::<crate::codex::CleanupUnconfirmed>())
            {
                WorkerExit::Unknown
            } else {
                WorkerExit::Settled
            };
            (exit, result.map_err(ApiError::from))
        })
        .map_err(lifecycle_error)?;
    receiver.await.map_err(|_| {
        ApiError(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "模型列表观察中断；未重发请求。".into(),
        )
    })?
}

async fn discover_owned(state: Arc<AppState>, request: ModelRequest) -> Result<Json<Value>> {
    let settings = state.settings.read().unwrap().clone();
    let models = match request.executor.as_deref().unwrap_or("native") {
        "codex" => {
            let stop = state.lifecycle.signal();
            crate::codex::list_models_with_control(&settings.codex_binary, &|| stop.is_cancelled())
                .await?
        }
        "native" => {
            let source = request
                .provider_source
                .as_deref()
                .unwrap_or(&settings.provider_source);
            if source == "named" {
                if request.api_key.is_some()
                    || request.base_url.is_some()
                    || request.models_endpoint.is_some()
                {
                    return Err(anyhow::anyhow!("命名服务模型发现不接受地址或凭据覆盖"));
                }
                let id = request
                    .provider_id
                    .as_deref()
                    .or(settings.provider_id.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("请选择命名模型服务"))?;
                let selected =
                    crate::capabilities::provider_runtime::discovery(&state, id, &settings).await?;
                selected.check(&state)?;
                let request = fetch_models_with_provider(&selected.settings);
                tokio::pin!(request);
                let models = loop {
                    selected.check(&state)?;
                    tokio::select! {
                        result = &mut request => break result?,
                        _ = tokio::time::sleep(Duration::from_millis(25)) => {},
                    }
                };
                selected.check(&state)?;
                let models: Vec<Value> = models
                    .into_iter()
                    .filter(|row| {
                        row.as_object().is_some_and(|fields| {
                            fields.values().all(|value| {
                                value
                                    .as_str()
                                    .is_none_or(|text| selected.settings.redact(text) == text)
                            })
                        })
                    })
                    .collect();
                return Ok(Json(json!({"models":models})));
            }
            let provider = match source {
                "local_codex" => {
                    let models_endpoint = request.models_endpoint.unwrap_or_else(|| {
                        if settings.provider_source == source {
                            settings.models_endpoint.clone()
                        } else {
                            String::new()
                        }
                    });
                    crate::configuration::resolve(&crate::Settings {
                        provider_source: source.into(),
                        model: String::new(),
                        models_endpoint,
                        ..settings.clone()
                    })?
                }
                "manual" => {
                    let url =
                        service_url(request.base_url.as_deref().unwrap_or(&settings.base_url))?;
                    let same_service = settings.provider_source == "manual"
                        && service_url(&settings.base_url).is_ok_and(|saved| {
                            url.as_str().trim_end_matches('/')
                                == saved.as_str().trim_end_matches('/')
                        });
                    crate::Settings {
                        provider_source: "manual".into(),
                        base_url: url.to_string(),
                        models_endpoint: request.models_endpoint.unwrap_or_else(|| {
                            if same_service {
                                settings.models_endpoint.clone()
                            } else {
                                String::new()
                            }
                        }),
                        api_key: request.api_key.unwrap_or_else(|| {
                            if same_service {
                                settings.api_key.clone()
                            } else {
                                String::new()
                            }
                        }),
                        provider_headers: Default::default(),
                        ..settings
                    }
                }
                _ => return Err(anyhow::anyhow!("模型配置来源无效")),
            };
            fetch_models_with_provider(&provider).await?
        }
        _ => return Err(anyhow::anyhow!("模型列表执行器无效")),
    };
    Ok(Json(json!({"models":models})))
}

#[cfg(test)]
async fn fetch_models(base: Url, key: &str) -> Result<Vec<Value>> {
    fetch_models_with_provider(&crate::Settings {
        base_url: base.to_string(),
        api_key: key.into(),
        ..Default::default()
    })
    .await
}

async fn fetch_models_with_provider(provider: &crate::Settings) -> Result<Vec<Value>> {
    let endpoint = models_url(&provider.base_url, &provider.models_endpoint)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(12))
        .build()
        .context("无法初始化模型列表连接")?;
    let request = provider.authorize(client.get(endpoint));
    let mut response = request.send().await.map_err(|_| {
        anyhow::anyhow!("无法获取模型列表，请检查服务地址和连接，或手动填写模型 ID")
    })?;
    if !response.status().is_success() {
        bail!(
            "模型列表服务返回 HTTP {}，请检查模型列表端点和密钥，也可手动填写模型 ID",
            response.status().as_u16()
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("读取模型列表失败"))?
    {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            bail!("模型列表响应超过 2 MiB 限制");
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).context("模型列表服务返回了无效 JSON")?;
    let rows = value["data"]
        .as_array()
        .context("模型列表响应缺少 data 数组，也可手动填写模型 ID")?;
    normalize_models(rows, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_endpoints_resolve_against_the_service_directory_and_reject_other_origins() {
        let base = "https://models.example/v1";
        for (endpoint, expected) in [
            ("", "https://models.example/v1/models"),
            ("catalog", "https://models.example/v1/catalog"),
            ("/catalog", "https://models.example/catalog"),
            (
                "https://models.example/custom?api-version=1",
                "https://models.example/custom?api-version=1",
            ),
        ] {
            assert_eq!(models_url(base, endpoint).unwrap().as_str(), expected);
        }
        for endpoint in [
            "https://other.example/catalog",
            "//other.example/catalog",
            "http://models.example/catalog",
            "https://models.example:1234/catalog",
            "https://actor:private-fixture-secret@models.example/catalog",
            "/catalog#private-fixture-secret",
            "file:///catalog",
            "catalog\nprivate-fixture-secret",
        ] {
            let error = models_url(base, endpoint).unwrap_err().to_string();
            assert!(!error.contains("private-fixture-secret"));
        }
    }

    #[test]
    fn catalog_uses_real_model_ids_and_ignores_hidden_codex_entries() {
        let rows = vec![
            json!({"id":"internal", "model":"provider/model", "displayName":"可用模型"}),
            json!({"id":"secret", "model":"hidden", "hidden":true}),
            json!({"model":"provider/model", "displayName":"可用模型"}),
            json!({"id":"\ninvalid", "model":"\n"}),
        ];
        let models = normalize_models(&rows, true).unwrap();
        assert_eq!(
            models,
            vec![json!({"id":"provider/model","name":"可用模型"})]
        );
        let native = normalize_models(
            &[json!({"id":"b"}), json!({"id":"a"}), json!({"id":"a"})],
            false,
        )
        .unwrap();
        assert_eq!(native.len(), 2);
        assert_eq!(native[0]["id"], "a");
    }

    #[tokio::test]
    async fn discovery_keeps_drafts_unsaved_and_does_not_forward_old_credentials() {
        use axum::{http::HeaderMap, routing::get, Router};
        let state = AppState::open(std::path::Path::new(":memory:"), "test".into()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                assert!(headers.get("authorization").is_none());
                Json(json!({"data":[{"id":"fixture-model"}]}))
            }),
        );
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        state.settings.write().unwrap().api_key = "saved-secret".into();
        let old = state.settings.read().unwrap().public();
        let Json(result) = discover(
            State(state.clone()),
            Json(ModelRequest {
                base_url: Some(base),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["models"][0]["id"], "fixture-model");
        assert_eq!(state.settings.read().unwrap().public(), old);
        server.abort();
    }

    #[tokio::test]
    async fn discovery_requests_the_custom_endpoint_and_uses_only_matching_service_credentials() {
        use axum::{extract::OriginalUri, http::HeaderMap, routing::get, Router};
        let state = AppState::open(std::path::Path::new(":memory:"), "test".into()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/v1/catalog",
                get(
                    |headers: HeaderMap, OriginalUri(uri): OriginalUri| async move {
                        assert_eq!(headers["authorization"], "Bearer saved-fixture-secret");
                        assert_eq!(uri.query(), Some("api-version=2026"));
                        Json(json!({"data":[{"id":"custom-model"}]}))
                    },
                ),
            )
            .route(
                "/different/catalog",
                get(|headers: HeaderMap| async move {
                    assert!(headers.get("authorization").is_none());
                    Json(json!({"data":[{"id":"new-service-model"}]}))
                }),
            )
            .route(
                "/different/auth-catalog",
                get(|headers: HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer fresh-fixture-secret");
                    Json(json!({"data":[{"id":"fresh-credential-model"}]}))
                }),
            );
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        {
            let mut settings = state.settings.write().unwrap();
            settings.base_url = base.clone();
            settings.provider_source = "manual".into();
            settings.models_endpoint = "catalog?api-version=2026".into();
            settings.api_key = "saved-fixture-secret".into();
        }
        let old = state.settings.read().unwrap().public();
        let Json(result) = discover(State(state.clone()), Json(ModelRequest::default()))
            .await
            .unwrap();
        assert_eq!(result["models"][0]["id"], "custom-model");
        let Json(result) = discover(
            State(state.clone()),
            Json(ModelRequest {
                base_url: Some(base.replace("/v1", "/different")),
                models_endpoint: Some("catalog".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["models"][0]["id"], "new-service-model");
        let Json(result) = discover(
            State(state.clone()),
            Json(ModelRequest {
                base_url: Some(base.replace("/v1", "/different")),
                models_endpoint: Some("/different/auth-catalog".into()),
                api_key: Some("fresh-fixture-secret".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(result["models"][0]["id"], "fresh-credential-model");
        assert_eq!(state.settings.read().unwrap().public(), old);
        server.abort();
    }

    #[tokio::test]
    async fn custom_endpoint_rejects_cross_origin_before_sending_credentials() {
        let provider = crate::Settings {
            base_url: "https://models.example/v1".into(),
            models_endpoint: "https://other.example/catalog".into(),
            api_key: "private-fixture-secret".into(),
            ..Default::default()
        };
        let error = fetch_models_with_provider(&provider)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("同源"));
        assert!(!error.contains("private-fixture-secret"));
    }

    #[tokio::test]
    async fn failed_catalog_does_not_expose_upstream_errors_or_follow_redirects() {
        use axum::{http::StatusCode, response::Redirect, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/error/models",
                get(|| async { (StatusCode::UNAUTHORIZED, "private-upstream-detail") }),
            )
            .route(
                "/redirect/models",
                get(|| async { Redirect::temporary("/unexpected") }),
            )
            .route(
                "/unexpected",
                get(|| async { Json(json!({"data":[{"id":"unexpected"}]})) }),
            );
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        for path in ["error", "redirect"] {
            let error = fetch_models(service_url(&format!("{base}/{path}")).unwrap(), "secret")
                .await
                .unwrap_err();
            assert!(error.to_string().contains("HTTP"));
            assert!(!error.to_string().contains("private-upstream-detail"));
        }
        server.abort();
    }
}
