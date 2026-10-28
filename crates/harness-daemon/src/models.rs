use anyhow::{bail, Context, Result};
use axum::{extract::State, Json};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

use crate::{ApiResult, AppState};

#[derive(Default, Deserialize)]
pub(crate) struct ModelRequest {
    base_url: Option<String>,
    api_key: Option<String>,
    executor: Option<String>,
    provider_source: Option<String>,
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
    let settings = state.settings.read().unwrap().clone();
    let models = match request.executor.as_deref().unwrap_or("native") {
        "codex" => crate::codex::list_models(&settings.codex_binary).await?,
        "native" => {
            let source = request
                .provider_source
                .as_deref()
                .unwrap_or(&settings.provider_source);
            let provider = match source {
                "local_codex" => crate::configuration::resolve(&crate::Settings {
                    provider_source: source.into(),
                    model: String::new(),
                    ..settings.clone()
                })?,
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
                _ => return Err(anyhow::anyhow!("模型配置来源无效").into()),
            };
            fetch_models_with_provider(&provider).await?
        }
        _ => return Err(anyhow::anyhow!("模型列表执行器无效").into()),
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
    let endpoint = format!("{}/models", provider.base_url.trim_end_matches('/'));
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
            "模型列表服务返回 HTTP {}，请检查地址和密钥；服务需支持 GET /models，也可手动填写模型 ID",
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
