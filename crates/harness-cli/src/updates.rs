//! Explicit local-owner verifier/stager client. No network source selection,
//! artifact execution, installation, upload retry or implicit trust enrollment.
use super::{Client, Options};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path, time::Duration};
const PREFIX: &str = "/api/v1/reliability/updates";
const HELP: &str = r#"本机签名验证与私有暂存（不是安装器）
  updates status
  updates trust <JSON文件|->
  updates verify <配置修订> <原始清单文件> <签名hex文件> <产物文件>
  updates operation <原操作UUID>
  updates cancel <原操作UUID>
  updates discard <原操作UUID> <原产物SHA256>

trust JSON字段：expected_revision（首次null）、key_id、source（固定HTTPS标识）、
public_key_hex（32字节公钥hex）、enabled、confirmation_fingerprint（公钥SHA256）。
先从独立可信来源核对指纹；验证请求不能自行选择公钥。生产自动更新始终禁用。
verify 原样签名清单<=16KiB，签名64字节小写hex，单产物<=256MiB；
可用全局--command-id指定原UUID。仅一次POST意图及一次PUT字节，绝不自动重发。
结果未知时用operation只读查询；重复创建新UUID不是恢复方式。
这里只读取用户指定的本机文件并私有暂存，不下载、解压、执行或安装。
"#;
fn rejected() -> anyhow::Error {
    anyhow::anyhow!("更新验证请求未确认；只读查询原操作，不要重发。未回显路径、输入或底层错误。")
}
fn uuid(v: &str) -> Result<()> {
    super::provider_uuid(v).map_err(|_| rejected())
}
fn digest(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn read_bounded(path: &str, max: usize, stdin: bool) -> Result<String> {
    let mut bytes = Vec::new();
    if path == "-" {
        if !stdin {
            bail!("此输入必须为本机文件，不能与其他输入共享标准输入");
        }
        std::io::stdin()
            .take((max + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| rejected())?;
    } else {
        let file = std::fs::File::open(path).map_err(|_| rejected())?;
        if !file.metadata().map_err(|_| rejected())?.is_file() {
            return Err(rejected());
        }
        file.take((max + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| rejected())?;
    }
    if bytes.len() > max {
        return Err(rejected());
    }
    String::from_utf8(bytes).map_err(|_| rejected())
}
async fn response(mut response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() || response.content_length().is_some_and(|n| n > 512 * 1024)
    {
        return Err(rejected());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| rejected())? {
        if chunk.len() > 512 * 1024 - bytes.len() {
            return Err(rejected());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| rejected())
}
fn request(
    client: &Client,
    method: reqwest::Method,
    path: &str,
) -> Result<reqwest::RequestBuilder> {
    Ok(client
        .http
        .request(method, client.base.join(path)?)
        .bearer_auth(&client.token)
        .header(
            "X-Self-Agent-Protocol",
            client
                .protocol
                .get()
                .copied()
                .context("尚未协商后台协议")?
                .to_string(),
        ))
}
async fn json_request(
    client: &Client,
    method: reqwest::Method,
    path: &str,
    body: Option<String>,
) -> Result<Value> {
    let mut request = request(client, method, path)?;
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    response(request.send().await.map_err(|_| rejected())?).await
}
fn same_receipt(v: &Value, id: &str, revision: u64, manifest: &Value, raw_sha256: &str) -> bool {
    v["id"] == id
        && v["config_revision"].as_u64() == Some(revision)
        && [
            "pending",
            "verifying",
            "verified",
            "failed",
            "interrupted",
            "unknown",
        ]
        .contains(&v["status"].as_str().unwrap_or(""))
        && v["metadata"]["manifest_sha256"] == raw_sha256
        && v["metadata"]["version"] == manifest["version"]
        && v["metadata"]["sequence"] == manifest["sequence"]
        && v["metadata"]["target"] == manifest["target"]
        && v["metadata"]["bytes"] == manifest["artifact"]["bytes"]
        && v["metadata"]["sha256"] == manifest["artifact"]["sha256"]
}
async fn verify(client: &Client, options: &Options) -> Result<Value> {
    let a = &options.args;
    let revision = a[1]
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0 && *v <= 9_007_199_254_740_991)
        .context("信任配置修订无效")?;
    uuid(&options.command_id)?;
    let raw = read_bounded(&a[2], 16 * 1024, false)?;
    let raw_sha256 = format!("{:x}", Sha256::digest(raw.as_bytes()));
    let signature = read_bounded(&a[3], 130, false)?
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    if signature.len() != 128
        || !signature
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(rejected());
    }
    let manifest: Value = serde_json::from_str(&raw).map_err(|_| rejected())?;
    let expected = manifest["artifact"]["bytes"]
        .as_u64()
        .filter(|v| *v > 0 && *v <= 256 * 1024 * 1024)
        .ok_or_else(rejected)?;
    let file = tokio::fs::File::open(Path::new(&a[4]))
        .await
        .map_err(|_| rejected())?;
    let metadata = file.metadata().await.map_err(|_| rejected())?;
    if !metadata.is_file() || metadata.len() != expected {
        return Err(rejected());
    }
    eprintln!(
        "验证操作 UUID：{}；未知时仅用 updates operation 查询，不自动重发。",
        options.command_id
    );
    let accepted=json_request(client,reqwest::Method::POST,&format!("{PREFIX}/verifications"),Some(json!({"id":options.command_id,"config_revision":revision,"raw_manifest":raw,"signature_hex":signature}).to_string())).await?;
    if !same_receipt(
        &accepted,
        &options.command_id,
        revision,
        &manifest,
        &raw_sha256,
    ) {
        return Err(rejected());
    }
    if accepted["status"] != "pending" {
        eprintln!("这是已有操作的历史回执；没有再次上传或把历史成功当作当前暂存证明。");
        return Ok(accepted);
    }
    let uploaded = request(
        client,
        reqwest::Method::PUT,
        &format!("{PREFIX}/verifications/{}/artifact", options.command_id),
    )?
    .timeout(Duration::from_secs(150))
    .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
    .header(reqwest::header::CONTENT_LENGTH, expected)
    .body(reqwest::Body::from(file))
    .send()
    .await
    .map_err(|_| rejected())?;
    let result = response(uploaded).await?;
    if !same_receipt(
        &result,
        &options.command_id,
        revision,
        &manifest,
        &raw_sha256,
    ) {
        return Err(rejected());
    }
    Ok(result)
}
/// Called only after the parent client has authenticated and negotiated the
/// exact daemon. Keeping that shared guard in Client covers every update route.
pub(super) fn help(options: &Options) -> bool {
    if options.command == "updates"
        && (options.args.as_slice() == ["--help"] || options.args.as_slice() == ["help"])
    {
        println!("{HELP}");
        true
    } else {
        false
    }
}
pub(super) async fn run(options: &Options, client: &Client) -> Result<bool> {
    if options.command != "updates" {
        return Ok(false);
    }
    if options.follow || options.wait.is_some() || options.controls.is_some() {
        bail!("updates不支持follow/wait/controls；未知结果仅只读查询");
    }
    let a = &options.args;
    if a.as_slice() == ["--help"] || a.as_slice() == ["help"] {
        println!("{HELP}");
        return Ok(true);
    }
    let result = match a.first().map(String::as_str) {
        None | Some("status") if a.len() <= 1 => {
            json_request(client, reqwest::Method::GET, PREFIX, None).await?
        }
        Some("trust") if a.len() == 2 => {
            let raw = read_bounded(&a[1], 8192, true)?;
            eprintln!("仅配置本机验证信任；不是安装授权。未知结果请用 updates status 核对配置修订，不自动重发。");
            json_request(
                client,
                reqwest::Method::POST,
                &format!("{PREFIX}/trust"),
                Some(raw),
            )
            .await?
        }
        Some("verify") if a.len() == 5 => verify(client, options).await?,
        Some("operation" | "cancel") if a.len() == 2 => {
            uuid(&a[1])?;
            let path = format!(
                "{PREFIX}/verifications/{}{}",
                a[1],
                if a[0] == "cancel" { "/cancel" } else { "" }
            );
            let value = json_request(
                client,
                if a[0] == "cancel" {
                    reqwest::Method::POST
                } else {
                    reqwest::Method::GET
                },
                &path,
                None,
            )
            .await?;
            let id = if a[0] == "cancel" {
                &value["id"]
            } else {
                &value["receipt"]["id"]
            };
            if id != &a[1] {
                return Err(rejected());
            }
            value
        }
        Some("discard") if a.len() == 3 => {
            uuid(&a[1])?;
            if !digest(&a[2]) {
                return Err(rejected());
            }
            let value = json_request(
                client,
                reqwest::Method::POST,
                &format!("{PREFIX}/stages/{}/discard", a[1]),
                Some(json!({"expected_sha256":a[2]}).to_string()),
            )
            .await?;
            if value["id"] != a[1] {
                return Err(rejected());
            }
            value
        }
        _ => bail!("updates参数无效；使用 updates --help 查看用法"),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    if a.first().is_some_and(|v| v == "verify") && result["status"] != "verified" {
        bail!("原验证操作未确认成功；没有再次上传、执行或安装");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_receipt_required_before_upload_and_hash_inputs_bounded() {
        let m = json!({"version":"1.0.1","sequence":1,"target":"linux-x86_64","artifact":{"bytes":12,"sha256":"a".repeat(64)}});
        let receipt = json!({"id":"op","config_revision":1,"status":"pending","metadata":{"version":"1.0.1","sequence":1,"target":"linux-x86_64","bytes":12,"sha256":"a".repeat(64),"manifest_sha256":"b".repeat(64)}});
        assert!(same_receipt(&receipt, "op", 1, &m, &"b".repeat(64)));
        assert!(!same_receipt(&receipt, "other", 1, &m, &"b".repeat(64)));
        assert!(!same_receipt(&receipt, "op", 2, &m, &"b".repeat(64)));
        assert!(!same_receipt(&json!({}), "op", 1, &m, &"b".repeat(64)));
        assert!(!same_receipt(&receipt, "op", 1, &m, &"c".repeat(64)));
        assert!(digest(&"a".repeat(64)));
        assert!(!digest(&"A".repeat(64)));
        assert!(!digest("../canary"));
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    #[tokio::test]
    async fn different_raw_manifest_receipt_never_triggers_artifact_upload() {
        let root =
            std::env::temp_dir().join(format!("update-cli-receipt-{}", harness_core::new_id()));
        std::fs::create_dir(&root).unwrap();
        let manifest = json!({"version":"1.0.1","sequence":1,"target":"linux-x86_64","artifact":{"bytes":1,"sha256":"a".repeat(64)}});
        let raw = manifest.to_string();
        let manifest_path = root.join("manifest");
        let signature_path = root.join("signature");
        let artifact_path = root.join("artifact");
        std::fs::write(&manifest_path, &raw).unwrap();
        std::fs::write(&signature_path, "0".repeat(128)).unwrap();
        std::fs::write(&artifact_path, b"x").unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        let receipt = json!({"id":id,"config_revision":1,"status":"pending","metadata":{"version":"1.0.1","sequence":1,"target":"linux-x86_64","bytes":1,"sha256":"a".repeat(64),"manifest_sha256":"b".repeat(64)}});
        let result=crate::tests::cli_fixture(&["--command-id",id,"updates","verify","1",manifest_path.to_str().unwrap(),signature_path.to_str().unwrap(),artifact_path.to_str().unwrap()],vec![("POST","/api/v1/reliability/updates/verifications",Some(json!({"id":id,"config_revision":1,"raw_manifest":raw,"signature_hex":"0".repeat(128)})),Some(receipt.to_string()))]).await;
        assert!(result.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
