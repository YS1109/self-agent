//! LOCAL TEST MATERIAL ONLY. Ephemeral process-memory Ed25519 key; no private
//! key output, enrollment, network, filesystem writes or artifact execution.
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
fn main() -> anyhow::Result<()> {
    if std::env::args().count() != 1 {
        anyhow::bail!("update_fixture 不接受路径、凭据或发布参数");
    }
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        ("macos", "aarch64") => "macos-aarch64",
        _ => anyhow::bail!("当前平台不在本地发布夹具范围"),
    };
    let key = Ed25519KeyPair::from_pkcs8(
        Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
            .map_err(|_| anyhow::anyhow!("临时测试密钥生成失败"))?
            .as_ref(),
    )
    .map_err(|_| anyhow::anyhow!("临时测试密钥无效"))?;
    let source = "https://updates.example.invalid/local-fixture/";
    let trust = json!({"expected_revision":null,"key_id":"ephemeral-fixture-only","source":source,"enabled":true,"public_key_hex":hex(key.public_key().as_ref()),"confirmation_fingerprint":format!("{:x}",Sha256::digest(key.public_key().as_ref()))});
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut records = Vec::<Value>::new();
    for sequence in 1..=2 {
        let artifact =
            format!("Self Agent inert local fixture {sequence}. Never execute this text.\n");
        let raw=json!({"manifest_schema":1,"application":"self-agent","channel":"stable","source":source,"key_id":"ephemeral-fixture-only","version":format!("9.9.{sequence}"),"sequence":sequence,"target":target,"database_schema_min":1,"database_schema_max":4,"issued_at":now.saturating_sub(1),"expires_at":now+600,"artifact":{"name":"inert-fixture.txt","bytes":artifact.len(),"sha256":format!("{:x}",Sha256::digest(artifact.as_bytes()))}}).to_string();
        let mut signed = b"self-agent-update-manifest-v1\0".to_vec();
        signed.extend_from_slice(raw.as_bytes());
        records.push(json!({"raw_manifest":raw,"signature_hex":hex(key.sign(&signed).as_ref()),"artifact_text":artifact}));
    }
    println!(
        "{}",
        json!({"fixture_schema":1,"trust":trust,"records":records})
    );
    Ok(())
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
