//! Local-owner gateway management only; not a remote browser RPC proxy.
use super::*;
fn stdin_secret(reader: impl Read) -> Result<String> {
    let secret = provider_stdin_secret(reader)?;
    if secret.len() != 64
        || !secret
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("设备凭据必须为64个小写十六进制字符；内容未回显");
    }
    Ok(secret)
}
fn revision(value: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0 && *v < 9_007_199_254_740_991)
        .context("网关 配置修订无效")
}
pub(super) fn request(options: &Options) -> Result<Option<(String, String, Option<Value>)>> {
    if options.command != "gateway" {
        return Ok(None);
    }
    if options.follow || options.wait.is_some() {
        bail!("网关 管理只支持一次提交或只读查询，不支持follow/wait");
    }
    let a = &options.args;
    if a.is_empty() || a.as_slice() == ["snapshot"] {
        return Ok(Some(("GET".into(), "/api/v1/gateway".into(), None)));
    }
    provider_uuid(&options.command_id)?;
    match a[0].as_str(){
 "receipt"|"credential-operation" if a.len()==2=>{provider_uuid(&a[1])?;Ok(Some(("GET".into(),format!("/api/v1/gateway/{}/{}",if a[0]=="receipt"{"commands"}else{"credential-operations"},a[1]),None)))},
 "command" if a.len()==2=>{let command:Value=serde_json::from_str(&input(&a[1],true)?).map_err(|_|anyhow::anyhow!("网关 管理指令JSON无效，内容未回显"))?;if !matches!(command["type"].as_str(),Some("configure"|"issue_pairing"|"confirm_binding"|"revoke_binding"))||command.get("secret").is_some(){bail!("网关 command仅接受配置、配对、确认或撤权元数据；凭据须从专用stdin入口输入");}Ok(Some(("POST".into(),"/api/v1/gateway/commands".into(),Some(json!({"id":options.command_id,"command":command})))))},
 "pair" if a.len()==2=>{let ttl=a[1].parse::<u32>().ok().filter(|v|(1..=600).contains(v)).context("网关 配对期限须为1至600秒")?;Ok(Some(("POST".into(),"/api/v1/gateway/commands".into(),Some(json!({"id":options.command_id,"command":{"type":"issue_pairing","ttl_seconds":ttl}})))))},
 "credential-set"|"credential-delete" if ((a[0]=="credential-delete"&&a.len()==3)||(a[0]=="credential-set"&&a.len()==4&&a[3]=="--stdin"))=>{provider_uuid(&a[1])?;let revision=revision(&a[2])?;let mut body=json!({"id":options.command_id,"expected_revision":revision});if a[0]=="credential-set"{body["secret"]=json!(stdin_secret(std::io::stdin())?);}Ok(Some((if a[0]=="credential-set"{"POST"}else{"DELETE"}.into(),format!("/api/v1/gateway/credentials/{}",a[1]),Some(body))))},
 "cleanup" if a.len()==2=>{provider_uuid(&a[1])?;Ok(Some(("POST".into(),format!("/api/v1/gateway/credentials/cleanup/{}",a[1]),Some(json!({"id":options.command_id})))))},
 _=>bail!("使用gateway snapshot/command FILE|-/receipt ID/pair TTL/credential-set CONFIG REV --stdin/credential-delete CONFIG REV/credential-operation ID/cleanup ID；不要在参数中填写凭据"),
 }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(args: &[&str]) -> Options {
        parse(args.iter().map(|v| (*v).into())).unwrap().unwrap()
    }
    #[test]
    fn exact_owner_routes_and_secret_stdin_only() {
        let id = "11111111-1111-4111-8111-111111111111";
        for (args, method, path) in [
            (vec!["gateway"], "GET", "/api/v1/gateway".into()),
            (
                vec!["gateway", "receipt", id],
                "GET",
                format!("/api/v1/gateway/commands/{id}"),
            ),
            (
                vec!["gateway", "credential-delete", id, "1"],
                "DELETE",
                format!("/api/v1/gateway/credentials/{id}"),
            ),
            (
                vec!["gateway", "cleanup", id],
                "POST",
                format!("/api/v1/gateway/credentials/cleanup/{id}"),
            ),
        ] {
            let value = request(&options(&args)).unwrap().unwrap();
            assert_eq!(value.0, method);
            assert_eq!(value.1, path);
        }
        for args in [
            vec!["gateway", "credential-set", id, "1", "SECRET_CANARY"],
            vec!["gateway", "credential-set", id, "1"],
            vec!["gateway", "credential-delete", id, "0"],
            vec!["gateway", "receipt", "../providers"],
            vec!["gateway", "pair", "601"],
            vec!["--follow", "gateway", "snapshot"],
        ] {
            let error = request(&options(&args)).unwrap_err().to_string();
            assert!(!error.contains("SECRET_CANARY"));
        }
        assert!(stdin_secret("a".repeat(64).as_bytes()).is_ok());
        assert!(stdin_secret("A".repeat(64).as_bytes()).is_err());
        assert!(stdin_secret(b"secret-canary".as_slice()).is_err());
    }
    #[tokio::test]
    async fn real_http_metadata_mutation_unknown_is_not_reposted() {
        use crate::tests::cli_fixture;
        let id = "11111111-1111-4111-8111-111111111111";
        cli_fixture(
            &["gateway", "snapshot"],
            vec![("GET", "/api/v1/gateway", None, Some("{}".into()))],
        )
        .await
        .unwrap();
        cli_fixture(
            &["--command-id", id, "gateway", "pair", "60"],
            vec![(
                "POST",
                "/api/v1/gateway/commands",
                Some(json!({"type":"issue_pairing","ttl_seconds":60})),
                None,
            )],
        )
        .await
        .unwrap_err();
    }
}
