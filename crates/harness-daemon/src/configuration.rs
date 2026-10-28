use std::{collections::HashMap, path::Path};

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::Settings;

pub(crate) fn default_source() -> String {
    "manual".into()
}

pub(crate) fn validate_model(model: &str) -> Result<()> {
    if looks_like_credential(model) {
        bail!("模型字段疑似访问密钥，请选择模型名称；凭据应放在访问密钥字段");
    }
    if model.len() > 200 || model.chars().any(char::is_control) {
        bail!("模型 ID 不能超过 200 字节或包含控制字符");
    }
    Ok(())
}

pub(crate) fn looks_like_credential(value: &str) -> bool {
    let value = value.trim();
    (value.starts_with("sk-") && value.len() >= 23)
        || value.starts_with("ghp_")
        || value.starts_with("github_pat_")
        || value.starts_with("Bearer ")
}

#[derive(Deserialize)]
struct CodexConfig {
    model: Option<String>,
    model_provider: Option<String>,
    // Profiles and project-local overrides are deliberately not merged.
    profile: Option<String>,
    #[serde(default)]
    model_providers: HashMap<String, CodexProvider>,
}

#[derive(Deserialize)]
struct CodexProvider {
    base_url: Option<String>,
    env_key: Option<String>,
    experimental_bearer_token: Option<String>,
    #[serde(default)]
    http_headers: HashMap<String, String>,
    #[serde(default)]
    env_http_headers: HashMap<String, String>,
    #[serde(default)]
    requires_openai_auth: bool,
}

pub(crate) fn resolve(settings: &Settings) -> Result<Settings> {
    let mut resolved = match settings.provider_source.as_str() {
        "manual" => settings.clone(),
        "local_codex" => {
            let home = std::env::var_os("CODEX_HOME")
                .map(std::path::PathBuf::from)
                .or_else(|| dirs::home_dir().map(|path| path.join(".codex")))
                .context("找不到本机 Codex 配置目录")?;
            from_codex_file(settings, &home.join("config.toml"), |name| {
                std::env::var(name).ok()
            })?
        }
        _ => bail!("模型配置来源无效"),
    };
    crate::models::models_url(&resolved.base_url, &resolved.models_endpoint)?;
    crate::model_config::validate_registry(&resolved.model_contexts)?;
    validate_model(&resolved.model)?;
    if resolved.model.trim().is_empty() {
        bail!("尚未选择主智能体模型");
    }
    resolved.provider_source = "manual".into();
    Ok(resolved)
}

fn from_codex_file(
    settings: &Settings,
    path: &Path,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<Settings> {
    let file = std::fs::File::open(path).context("无法读取本机 Codex config.toml")?;
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .context("无法读取本机 Codex 配置")?;
    if bytes.len() > 1024 * 1024 {
        bail!("本机 Codex 配置超过 1 MiB 限制");
    }
    // TOML errors may include the original line, which could contain a credential.
    let config: CodexConfig = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| toml::from_str(text).ok())
        .context("本机 Codex 配置格式无效，未回显原始内容")?;
    if config.profile.is_some() {
        bail!("本机 Codex 使用了 profile；当前复用仅支持顶层自定义服务配置");
    }
    let name = config
        .model_provider
        .context("本机 Codex 未选择自定义模型服务；ChatGPT 登录请使用 Codex 子任务执行器")?;
    let provider = config
        .model_providers
        .get(&name)
        .context("本机 Codex 选择的服务未在 model_providers 中定义")?;
    if provider.requires_openai_auth {
        bail!("该 Codex 服务使用专属登录，无法复用；请使用 Codex 子任务执行器");
    }
    let base = provider
        .base_url
        .as_deref()
        .context("本机 Codex 服务缺少地址")?;
    crate::models::service_url(base)?;
    let mut headers = HeaderMap::new();
    for (name, value) in &provider.http_headers {
        insert_header(&mut headers, name, value)?;
    }
    for (name, variable) in &provider.env_http_headers {
        let value = environment(variable).context("Codex 服务请求头所需的环境变量未设置")?;
        insert_header(&mut headers, name, &value)?;
    }
    let key = if let Some(variable) = &provider.env_key {
        environment(variable).context("Codex 服务密钥所需的环境变量未设置")?
    } else {
        provider
            .experimental_bearer_token
            .clone()
            .unwrap_or_default()
    };
    if !key.is_empty() {
        let mut value = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| anyhow::anyhow!("Codex 服务凭据格式无效"))?;
        value.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, value);
    }
    let mut resolved = settings.clone();
    resolved.base_url = base.trim_end_matches('/').to_owned();
    resolved.model = if settings.model.trim().is_empty() {
        config.model.context("本机 Codex 尚未选择模型")?
    } else {
        settings.model.clone()
    };
    validate_model(&resolved.model)?;
    resolved.api_key = key;
    resolved.provider_headers = headers;
    Ok(resolved)
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| anyhow::anyhow!("Codex 服务请求头名称无效"))?;
    if matches!(
        name.as_str(),
        "host" | "content-length" | "transfer-encoding" | "connection" | "content-type"
    ) {
        bail!("Codex 服务配置包含不能复用的传输请求头");
    }
    let mut value =
        HeaderValue::from_str(value).map_err(|_| anyhow::anyhow!("Codex 服务请求头格式无效"))?;
    value.set_sensitive(true);
    headers.insert(name, value);
    Ok(())
}

pub(crate) async fn check(settings: &Settings) -> Result<Value> {
    let resolved = resolve(settings)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(4))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let request = client
        .post(format!(
            "{}/chat/completions",
            resolved.base_url.trim_end_matches('/')
        ))
        .json(&json!({
            "model":resolved.model, "stream":false, "max_tokens":512,
            "response_format":{"type":"json_object"},
            "messages":[
                {"role":"system","content":"Return exactly this JSON object: {\"ok\":true}"},
                {"role":"user","content":"Check the connection. Do not use tools."}
            ]
        }));
    let mut response = resolved
        .authorize(request)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("模型连接检测失败，请检查服务是否运行、地址与鉴权"))?;
    if !response.status().is_success() {
        bail!(
            "模型连接检测返回 HTTP {}；请检查模型、鉴权及 Chat Completions 支持",
            response.status().as_u16()
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("检测响应读取失败"))?
    {
        if bytes.len() + chunk.len() > 256 * 1024 {
            bail!("检测响应超过 256 KiB 限制");
        }
        bytes.extend_from_slice(&chunk);
    }
    let body: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("检测响应不是有效 JSON"))?;
    let content = body
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str);
    let output = content.and_then(|value| serde_json::from_str::<Value>(value).ok());
    if output.as_ref().and_then(|value| value["ok"].as_bool()) != Some(true) {
        bail!("服务可连接，但未返回约定的 JSON；请检查模型的 JSON 输出支持");
    }
    Ok(
        json!({"ok":true, "message":"连接成功，所选模型已完成真实 JSON 回复；未创建任务或修改项目。"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_config(
        text: &str,
        settings: &Settings,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Settings> {
        let path =
            std::env::temp_dir().join(format!("self-agent-config-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, text).unwrap();
        let result = from_codex_file(settings, &path, env);
        std::fs::remove_file(path).unwrap();
        result
    }

    #[test]
    fn reuses_selected_provider_headers_and_credentials_without_serializing_them() {
        let config = r#"
model = "local/model"
model_provider = "selected"
[model_providers.selected]
base_url = "http://127.0.0.1:1234/v1"
experimental_bearer_token = "fixture-bearer-secret"
[model_providers.selected.http_headers]
x-actor = "fixture-actor-secret"
[model_providers.selected.env_http_headers]
x-session = "FIXTURE_SESSION"
[model_providers.other]
base_url = "https://unused.example/v1"
experimental_bearer_token = "unused-fixture-secret"
"#;
        let settings = Settings {
            model: String::new(),
            provider_source: "local_codex".into(),
            models_endpoint: "/catalog?api-version=2026".into(),
            ..Settings::default()
        };
        let resolved = with_config(config, &settings, |name| {
            (name == "FIXTURE_SESSION").then(|| "fixture-session-secret".into())
        })
        .unwrap();
        assert_eq!(resolved.model, "local/model");
        assert_eq!(resolved.models_endpoint, "/catalog?api-version=2026");
        assert_eq!(resolved.provider_headers["x-actor"], "fixture-actor-secret");
        let request = resolved
            .authorize(reqwest::Client::new().get("http://localhost"))
            .build()
            .unwrap();
        assert_eq!(
            request.headers()["authorization"],
            "Bearer fixture-bearer-secret"
        );
        let serialized = serde_json::to_string(&resolved).unwrap();
        for secret in [
            "fixture-bearer-secret",
            "fixture-actor-secret",
            "fixture-session-secret",
            "unused-fixture-secret",
        ] {
            assert!(!serialized.contains(secret));
            if secret != "unused-fixture-secret" {
                assert!(!resolved.redact(secret).contains(secret));
            }
        }
        // The file is re-read after a restart; no in-memory credential is required.
        let mut restarted: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        restarted.model = "override/model".into();
        let resolved = with_config(config, &restarted, |_| {
            Some("fixture-session-secret".into())
        })
        .unwrap();
        assert_eq!(resolved.model, "override/model");
        assert_eq!(resolved.api_key, "fixture-bearer-secret");
    }

    #[test]
    fn rejects_unsupported_or_invalid_local_configs_without_echoing_source() {
        for config in [
            "experimental_bearer_token = \"private-fixture-secret",
            "profile = 'work'\nmodel_provider = 'custom'",
            "model_provider = 'custom'\n[model_providers.custom]\nrequires_openai_auth = true",
            "model_provider = 'custom'\n[model_providers.custom]\nbase_url = 'http://user:private-fixture-secret@localhost'",
            "model_provider = 'custom'\n[model_providers.custom]\nbase_url = 'http://localhost'\nenv_key = 'MISSING'",
            "model_provider = 'custom'\n[model_providers.custom]\nbase_url = 'http://localhost'\nhttp_headers = { host = 'private-fixture-secret' }",
        ] {
            let error = with_config(config, &Settings::default(), |_| None).err().unwrap();
            assert!(!error.to_string().contains("private-fixture-secret"));
        }
    }

    #[tokio::test]
    async fn connection_check_uses_json_auth_and_reports_only_sanitized_diagnostics() {
        use axum::{
            extract::Json,
            http::{HeaderMap, StatusCode},
            routing::post,
            Router,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/v1/chat/completions",
                post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer fixture-secret");
                    assert_eq!(headers["x-actor"], "fixture-actor");
                    assert_eq!(body["response_format"]["type"], "json_object");
                    assert!(body.get("tools").is_none());
                    Json(json!({"choices":[{"message":{"content":"{\"ok\":true}"}}]}))
                }),
            )
            .route(
                "/error/chat/completions",
                post(|| async { (StatusCode::UNAUTHORIZED, "private-upstream-detail") }),
            );
        let server = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });
        let mut settings = Settings {
            base_url: base.clone(),
            model: "fixture-model".into(),
            api_key: "fixture-secret".into(),
            ..Settings::default()
        };
        insert_header(&mut settings.provider_headers, "x-actor", "fixture-actor").unwrap();
        assert_eq!(check(&settings).await.unwrap()["ok"], true);
        settings.base_url = base.replace("/v1", "/error");
        let error = check(&settings).await.unwrap_err().to_string();
        assert!(error.contains("401"));
        assert!(!error.contains("private-upstream-detail"));
        server.abort();
    }
}
