//! Read-only, bounded observations of a workspace execution interval.
//! Call capture after the project writer permit is acquired; call finish before
//! releasing it, including failed/cancelled runs. This does not prove authorship.
mod git;
use anyhow::{bail, Context, Result};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Component, Path, PathBuf},
};

const MAX_FILES: usize = 2000;
const MAX_DEPTH: usize = 24;
const MAX_FILE: usize = 256 * 1024;
const MAX_CAPTURE: usize = 8 * 1024 * 1024;
const MAX_REPORT_FILES: usize = 200;
const MAX_PATCH: usize = 32 * 1024;
const MAX_REPORT: usize = 512 * 1024;

/// Deliberately not Serialize or Debug: private before-images and known tokens
/// must never accidentally enter task JSON, logs or a frontend snapshot.
pub(crate) struct Baseline {
    root: PathBuf,
    directory: Dir,
    files: Scan,
    git: Value,
    secrets: Vec<String>,
}

#[derive(Default)]
struct Scan {
    files: BTreeMap<String, FileState>,
    sensitive: BTreeSet<String>,
    excluded: usize,
    visited: usize,
    bytes: usize,
    complete: bool,
}
struct FileState {
    kind: &'static str,
    bytes: u64,
    digest: Option<[u8; 32]>,
    git_oid: Option<[u8; 20]>,
    text: Option<String>,
    readonly: bool,
    mode: Option<u32>,
}

pub(crate) fn capture(root: &Path, known_secrets: &[String]) -> Result<Baseline> {
    let directory = open_root(root)?;
    let root = root.to_path_buf();
    if known_secrets.iter().map(String::len).sum::<usize>() > 512 * 1024 {
        bail!("敏感值配置超过改动检查预算");
    }
    let secrets: Vec<_> = known_secrets
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect();
    let matcher = crate::provider::SecretMatcher::new(&secrets);
    let files = scan(&directory, &matcher);
    let git = git::observe(&directory, &files, &matcher);
    Ok(Baseline {
        root,
        directory,
        files,
        git,
        secrets,
    })
}

pub(crate) fn finish(
    root: &Path,
    mut baseline: Baseline,
    additional_secrets: &[String],
) -> Result<Value> {
    baseline.secrets.extend(
        additional_secrets
            .iter()
            .filter(|secret| !secret.is_empty())
            .cloned(),
    );
    if baseline.secrets.iter().map(String::len).sum::<usize>() > 512 * 1024 {
        bail!("敏感值配置超过改动检查预算");
    }
    let matcher = crate::provider::SecretMatcher::new(&baseline.secrets);
    for (path, state) in &baseline.files.files {
        if state
            .text
            .as_ref()
            .is_some_and(|text| sensitive_content(text.as_bytes(), &matcher))
            || secret_in_path(path, &matcher)
        {
            baseline.files.sensitive.insert(path.clone());
        }
    }
    if root != baseline.root {
        bail!("项目目录已变化，不能与原改动基线比较");
    }
    let after = scan(&baseline.directory, &matcher);
    let hidden: BTreeSet<_> = baseline
        .files
        .sensitive
        .union(&after.sensitive)
        .cloned()
        .collect();
    let mut current_git = git::observe(&baseline.directory, &after, &matcher);
    let mut before_git = baseline.git;
    // A formerly safe file can become sensitive during execution. Never retain
    // its earlier name/status in a report that otherwise excludes that file.
    for observation in [&mut before_git, &mut current_git] {
        if let Some(entries) = observation["entries"].as_array_mut() {
            entries.retain(|entry| {
                entry["path"]
                    .as_str()
                    .is_some_and(|p| !hidden.contains(p) && !secret_in_path(p, &matcher))
            });
        }
    }
    let mut changes = Vec::new();
    let mut skipped = Vec::new();
    let mut report_bytes = 0;
    let mut truncated = false;
    let paths: BTreeSet<_> = baseline
        .files
        .files
        .keys()
        .chain(after.files.keys())
        .collect();
    let mut removed: BTreeMap<[u8; 32], Vec<&str>> = BTreeMap::new();
    let mut added: BTreeMap<[u8; 32], Vec<&str>> = BTreeMap::new();
    if baseline.files.complete && after.complete {
        for path in &paths {
            if hidden.contains(*path) {
                continue;
            }
            match (baseline.files.files.get(*path), after.files.get(*path)) {
                (Some(old), None) if old.digest.is_some() => {
                    removed.entry(old.digest.unwrap()).or_default().push(path)
                }
                (None, Some(new)) if new.digest.is_some() => {
                    added.entry(new.digest.unwrap()).or_default().push(path)
                }
                _ => {}
            }
        }
    }
    let mut renamed = BTreeSet::new();
    for (digest, old_paths) in &removed {
        if let Some(new_paths) = added.get(digest) {
            if old_paths.len() == 1 && new_paths.len() == 1 {
                let old = old_paths[0];
                let new = new_paths[0];
                let value = json!({"path":new,"previous_path":old,"status":"renamed",
                    "comparison":"identical_content","binary":after.files[new].kind=="binary",
                    "before_bytes":baseline.files.files[old].bytes,"after_bytes":after.files[new].bytes});
                if push_bounded(&mut changes, value, &mut report_bytes) {
                    renamed.insert(old);
                    renamed.insert(new);
                } else {
                    truncated = true;
                }
            }
        }
    }
    for path in paths {
        if hidden.contains(path) || renamed.contains(path.as_str()) {
            continue;
        }
        let old = baseline.files.files.get(path);
        let new = after.files.get(path);
        let uncertain =
            (old.is_none() && !baseline.files.complete) || (new.is_none() && !after.complete);
        let unavailable = old
            .into_iter()
            .chain(new)
            .find(|state| state.digest.is_none());
        if uncertain || unavailable.is_some() {
            let reason = if uncertain {
                "scan_incomplete"
            } else {
                unavailable.unwrap().kind
            };
            if !push_bounded(
                &mut skipped,
                json!({"path":path,"reason":reason}),
                &mut report_bytes,
            ) {
                truncated = true;
            }
            continue;
        }
        if let (Some(a), Some(b)) = (old, new) {
            if a.digest == b.digest && a.readonly == b.readonly && a.mode == b.mode {
                continue;
            }
        }
        let status = match (old, new) {
            (None, _) => "added",
            (_, None) => "deleted",
            _ => "modified",
        };
        let binary = old
            .into_iter()
            .chain(new)
            .any(|entry| entry.kind == "binary");
        let mut value = json!({"path":path,"status":status,"binary":binary,
            "before_bytes":old.map(|s| s.bytes),"after_bytes":new.map(|s| s.bytes),
            "before_readonly":old.map(|s| s.readonly),"after_readonly":new.map(|s| s.readonly),
            "before_mode":old.and_then(|s| s.mode),"after_mode":new.and_then(|s| s.mode)});
        if binary {
            value["diff_state"] = json!("binary_not_shown");
        } else {
            match unified_diff(
                path,
                old.and_then(|s| s.text.as_deref()),
                new.and_then(|s| s.text.as_deref()),
            ) {
                Some(patch) => {
                    value["diff_state"] = json!("available");
                    value["diff"] = json!(patch);
                }
                None => {
                    value["diff_state"] = json!("diff_budget_exceeded");
                    truncated = true;
                }
            }
        }
        if !push_bounded(&mut changes, value, &mut report_bytes) {
            truncated = true;
        }
    }
    let mut report = json!({
        "version":1,"scope":"observed_execution_interval","coverage":"filtered_workspace","attribution":"not_exclusive",
        "notice":"以下为取得项目执行权后至结束时观察到的变化；外部程序也可能修改工作区，不能全部归因于本任务。此视图只读，不会暂存、重置或回滚文件。",
        "baseline_git":before_git,"current_git":current_git,"files":changes,"skipped":skipped,
        "partial":truncated || !baseline.files.complete || !after.complete || !skipped.is_empty()
            || matches!(before_git["state"].as_str(), Some("unavailable" | "partial")) || matches!(current_git["state"].as_str(), Some("unavailable" | "partial")),
        "excluded_count":baseline.files.excluded.max(after.excluded),
        "sensitive_file_count":hidden.len(),
        "limits":{"files":MAX_FILES,"file_bytes":MAX_FILE,"capture_bytes":MAX_CAPTURE,
            "report_entries":MAX_REPORT_FILES,"patch_bytes":MAX_PATCH,"report_bytes":MAX_REPORT},
        "exclusions":"隐藏、敏感及生成目录不读取；符号链接不跟随。已知凭据和明显私钥文本所在文件不展示；这不是通用秘密检测保证。不会启动 Git 命令，因此仓库暂存区和工作树状态另行标为不可用。"
    });
    enforce_report_budget(&mut report);
    Ok(report)
}

fn enforce_report_budget(report: &mut Value) {
    // Include status arrays and JSON escaping in the final wire/storage bound.
    while report["files"].as_array().map_or(0, Vec::len)
        + report["skipped"].as_array().map_or(0, Vec::len)
        > MAX_REPORT_FILES
    {
        report["partial"] = json!(true);
        if report["skipped"]
            .as_array_mut()
            .is_some_and(|entries| entries.pop().is_some())
        {
            continue;
        }
        if let Some(entries) = report["files"].as_array_mut() {
            entries.pop();
        }
    }
    while serde_json::to_vec(report)
        .map(|bytes| bytes.len())
        .unwrap_or(MAX_REPORT + 1)
        > MAX_REPORT
    {
        report["partial"] = json!(true);
        let mut removed = false;
        for field in ["baseline_git", "current_git"] {
            if let Some(entries) = report[field]["entries"].as_array_mut() {
                if entries.pop().is_some() {
                    report[field]["state"] = json!("partial");
                    removed = true;
                    break;
                }
            }
        }
        if !removed {
            for field in ["skipped", "files"] {
                if report[field]
                    .as_array_mut()
                    .is_some_and(|entries| entries.pop().is_some())
                {
                    removed = true;
                    break;
                }
            }
        }
        if !removed {
            break;
        }
    }
}

fn push_bounded(output: &mut Vec<Value>, value: Value, size: &mut usize) -> bool {
    let bytes = serde_json::to_vec(&value)
        .map(|v| v.len())
        .unwrap_or(MAX_REPORT + 1);
    if output.len() >= MAX_REPORT_FILES || *size + bytes > MAX_REPORT {
        return false;
    }
    *size += bytes;
    output.push(value);
    true
}

/// The saved root is already canonical at project selection. Do not canonicalize
/// it again here: a queued root or ancestor may have been replaced by a symlink.
/// Pin the directory for the entire interval, including its Git observations.
fn open_root(root: &Path) -> Result<Dir> {
    if !root.is_absolute() {
        bail!("项目根目录必须是绝对路径");
    }
    let mut anchor = PathBuf::new();
    let mut names = Vec::new();
    for component in root.components() {
        match component {
            Component::Prefix(prefix) => {
                #[cfg(windows)]
                if !matches!(
                    prefix.kind(),
                    std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
                ) {
                    bail!("改动检查不接受远程或设备根目录");
                }
                anchor.push(prefix.as_os_str());
            }
            Component::RootDir => anchor.push(component.as_os_str()),
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => bail!("项目根目录不能包含上级路径"),
        }
    }
    let mut directory = Dir::open_ambient_dir(anchor, cap_std::ambient_authority())
        .context("无法打开项目所在文件系统")?;
    for name in names {
        directory = directory
            .open_dir_nofollow(name)
            .context("项目目录不可用或包含符号链接")?;
    }
    Ok(directory)
}

fn scan(dir: &Dir, secrets: &crate::provider::SecretMatcher) -> Scan {
    let mut scan = Scan {
        complete: true,
        ..Scan::default()
    };
    walk(dir, "", 0, secrets, &mut scan);
    scan
}

fn walk(
    dir: &Dir,
    prefix: &str,
    depth: usize,
    secrets: &crate::provider::SecretMatcher,
    scan: &mut Scan,
) {
    if depth > MAX_DEPTH || scan.visited >= MAX_FILES {
        scan.complete = false;
        return;
    }
    let Ok(entries) = dir.entries() else {
        scan.complete = false;
        return;
    };
    let mut names = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            scan.complete = false;
            continue;
        };
        if names.len() >= MAX_FILES {
            scan.complete = false;
            break;
        }
        names.push(entry.file_name());
    }
    names.sort();
    for name in names {
        if scan.visited >= MAX_FILES {
            scan.complete = false;
            break;
        }
        scan.visited += 1;
        let Some(name_text) = name.to_str() else {
            scan.excluded += 1;
            continue;
        };
        let path = if prefix.is_empty() {
            name_text.to_owned()
        } else {
            format!("{prefix}/{name_text}")
        };
        if excluded_path(&path) {
            scan.excluded += 1;
            continue;
        }
        if secret_in_path(&path, secrets) {
            scan.sensitive.insert(path);
            scan.excluded += 1;
            continue;
        }
        let Ok(metadata) = dir.symlink_metadata(&name) else {
            scan.complete = false;
            continue;
        };
        if metadata.file_type().is_symlink() {
            scan.files
                .insert(path, unavailable("symlink_not_followed", metadata.len()));
        } else if metadata.is_dir() {
            match dir.open_dir_nofollow(&name) {
                Ok(child) => walk(&child, &path, depth + 1, secrets, scan),
                Err(_) => {
                    scan.complete = false;
                }
            }
        } else if metadata.is_file() {
            let length = metadata.len();
            if length > MAX_FILE as u64 {
                scan.files
                    .insert(path, unavailable("file_too_large", length));
                continue;
            }
            if scan.bytes + length as usize > MAX_CAPTURE {
                scan.files
                    .insert(path, unavailable("capture_budget_exceeded", length));
                scan.complete = false;
                continue;
            }
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No).nonblock(true);
            let Ok(file) = dir.open_with(&name, &options) else {
                scan.files
                    .insert(path, unavailable("unreadable_or_changed", length));
                continue;
            };
            let Ok(opened) = file.metadata() else {
                scan.complete = false;
                continue;
            };
            if !opened.is_file() {
                scan.files
                    .insert(path, unavailable("not_regular_file", length));
                continue;
            }
            let mut bytes = Vec::new();
            let remaining = MAX_CAPTURE.saturating_sub(scan.bytes);
            let read_limit = (MAX_FILE + 1).min(remaining + 1);
            let read_result = (&file).take(read_limit as u64).read_to_end(&mut bytes);
            scan.bytes += bytes.len();
            if read_result.is_err() {
                scan.files
                    .insert(path, unavailable("unreadable_or_changed", length));
                continue;
            }
            let consistent = file.metadata().ok().is_some_and(|after| {
                after.len() == opened.len()
                    && after.modified().ok() == opened.modified().ok()
                    && bytes.len() as u64 == opened.len()
            });
            if !consistent {
                scan.files
                    .insert(path, unavailable("changed_during_capture", length));
                continue;
            }
            if bytes.len() > MAX_FILE || scan.bytes > MAX_CAPTURE {
                scan.files.insert(
                    path,
                    unavailable("capture_budget_exceeded", bytes.len() as u64),
                );
                scan.complete = false;
                continue;
            }
            if sensitive_content(&bytes, secrets) {
                scan.sensitive.insert(path);
                scan.excluded += 1;
                continue;
            }
            let text = if bytes.contains(&0) {
                None
            } else {
                String::from_utf8(bytes.clone()).ok()
            };
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            scan.files.insert(
                path,
                FileState {
                    kind: if text.is_some() { "text" } else { "binary" },
                    bytes: bytes.len() as u64,
                    digest: Some(digest),
                    git_oid: Some(git::blob_oid(&bytes)),
                    text,
                    readonly: opened.permissions().readonly(),
                    mode: file_mode(&opened),
                },
            );
        } else {
            scan.files
                .insert(path, unavailable("not_regular_file", metadata.len()));
        }
    }
}

fn unavailable(kind: &'static str, bytes: u64) -> FileState {
    FileState {
        kind,
        bytes,
        digest: None,
        git_oid: None,
        text: None,
        readonly: false,
        mode: None,
    }
}
fn excluded_path(path: &str) -> bool {
    Path::new(path)
        .components()
        .any(|component| match component {
            Component::Normal(name) => {
                let Some(name) = name.to_str() else {
                    return true;
                };
                let lower = name.to_ascii_lowercase();
                let stem = lower.split('.').next().unwrap_or("");
                name.starts_with('.')
                    || matches!(
                        stem,
                        "credential"
                            | "credentials"
                            | "secret"
                            | "secrets"
                            | "token"
                            | "tokens"
                            | "password"
                            | "passwords"
                            | "id_rsa"
                            | "id_dsa"
                            | "id_ecdsa"
                            | "id_ed25519"
                    )
                    || name.chars().any(char::is_control)
                    || matches!(
                        lower.as_str(),
                        "node_modules"
                            | "target"
                            | "dist"
                            | "build"
                            | "coverage"
                            | "vendor"
                            | "env"
                            | "credentials"
                            | "secrets"
                            | "auth.json"
                            | "id_rsa"
                            | "id_ed25519"
                    )
                    || [".pem", ".key", ".p12", ".pfx", ".keystore", ".jks", ".env"]
                        .iter()
                        .any(|suffix| lower.ends_with(suffix))
            }
            _ => true,
        })
}
fn secret_in_path(path: &str, secrets: &crate::provider::SecretMatcher) -> bool {
    secrets.contains_text(path, 0)
}

fn file_mode(metadata: &cap_std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        Some(metadata.mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}
fn sensitive_content(bytes: &[u8], secrets: &crate::provider::SecretMatcher) -> bool {
    secrets.contains_text(&String::from_utf8_lossy(bytes), 0)
        || (bytes
            .windows(b"-----BEGIN".len())
            .any(|window| window == b"-----BEGIN")
            && bytes
                .windows(b"PRIVATE KEY-----".len())
                .any(|window| window == b"PRIVATE KEY-----"))
}

/// A single exact replacement hunk (with three context lines) avoids an
/// unbounded quadratic LCS. It is intentionally not a minimal edit script.
fn unified_diff(path: &str, old: Option<&str>, new: Option<&str>) -> Option<String> {
    let a: Vec<_> = old.unwrap_or("").split_inclusive('\n').collect();
    let b: Vec<_> = new.unwrap_or("").split_inclusive('\n').collect();
    let prefix = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    if prefix == a.len() && prefix == b.len() {
        return Some(String::new());
    }
    let start = prefix.saturating_sub(3);
    let a_end = (a.len() - suffix + 3).min(a.len());
    let b_end = (b.len() - suffix + 3).min(b.len());
    let mut result = format!(
        "--- {}\n+++ {}\n@@ -{},{} +{},{} @@\n",
        if old.is_some() {
            format!("a/{path}")
        } else {
            "/dev/null".into()
        },
        if new.is_some() {
            format!("b/{path}")
        } else {
            "/dev/null".into()
        },
        if a_end == start { start } else { start + 1 },
        a_end - start,
        if b_end == start { start } else { start + 1 },
        b_end - start
    );
    let mut line = |sign: char, text: &str| {
        result.push(sign);
        result.push_str(text);
        if !text.ends_with('\n') {
            result.push_str("\n\\ No newline at end of file\n");
        }
        result.len() <= MAX_PATCH
    };
    for text in &a[start..prefix] {
        if !line(' ', text) {
            return None;
        }
    }
    for text in &a[prefix..a.len() - suffix] {
        if !line('-', text) {
            return None;
        }
    }
    for text in &b[prefix..b.len() - suffix] {
        if !line('+', text) {
            return None;
        }
    }
    for text in &a[a.len() - suffix..a_end] {
        if !line(' ', text) {
            return None;
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("self-agent-changes-{}", harness_core::new_id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
        fn write(&self, path: &str, text: impl AsRef<[u8]>) {
            std::fs::create_dir_all(self.0.join(path).parent().unwrap()).unwrap();
            std::fs::write(self.0.join(path), text).unwrap();
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn json_text_credentials_discovered_late_are_excluded_from_interval_reports() {
        let root = Temp::new();
        let secret = "known\"私有\\key\n尾";
        root.write("ordinary.txt", json!({"token":secret}).to_string());
        let baseline = capture(&root.0, &[]).unwrap();
        root.write("ordinary.txt", "now harmless");
        let report = finish(&root.0, baseline, &[secret.into()]).unwrap();
        let wire = report.to_string();
        assert!(!wire.contains("ordinary.txt"));
        assert!(!crate::provider::contains_known_secret(
            &report,
            &[secret.into()]
        ));
    }

    #[cfg(unix)]
    #[test]
    fn changed_root_links_are_rejected_and_interval_keeps_pinned_directory() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        temp.write("project/file.txt", "before");
        temp.write("outside/private.txt", "outside-marker-must-not-be-observed");
        let root = temp.0.join("project");
        let baseline = capture(&root, &[]).unwrap();
        std::fs::rename(&root, temp.0.join("original")).unwrap();
        symlink(temp.0.join("outside"), &root).unwrap();
        assert!(capture(&root, &[]).is_err());
        temp.write("original/file.txt", "after");
        let report = finish(&root, baseline, &[]).unwrap();
        let encoded = report.to_string();
        assert!(encoded.contains("file.txt"));
        assert!(encoded.contains("after"));
        assert!(!encoded.contains("private.txt"));
        assert!(!encoded.contains("outside-marker"));
        symlink(temp.0.join("outside"), temp.0.join("linked-parent")).unwrap();
        std::fs::create_dir(temp.0.join("outside/child")).unwrap();
        assert!(capture(&temp.0.join("linked-parent/child"), &[]).is_err());
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git fixture command failed: {args:?}"
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn preserves_existing_staged_unstaged_and_untracked_while_diffing_only_interval() {
        let root = Temp::new();
        git(&root.0, &["init", "-q"]);
        git(&root.0, &["config", "user.name", "Fixture"]);
        git(
            &root.0,
            &["config", "user.email", "fixture@example.invalid"],
        );
        root.write("main.txt", "original\n");
        git(&root.0, &["add", "main.txt"]);
        git(&root.0, &["commit", "-qm", "fixture"]);
        root.write("main.txt", "user staged\n");
        git(&root.0, &["add", "main.txt"]);
        root.write("main.txt", "user staged\nuser unstaged\n");
        root.write("user-untracked.txt", "user evidence");
        let index = std::fs::read(root.0.join(".git/index")).unwrap();
        let baseline = capture(&root.0, &[]).unwrap();
        root.write("main.txt", "user staged\nuser unstaged\nagent addition\n");
        root.write("new.txt", "new task file\n");
        let report = finish(&root.0, baseline, &[]).unwrap();
        assert_eq!(std::fs::read(root.0.join(".git/index")).unwrap(), index);
        assert_eq!(
            std::fs::read_to_string(root.0.join("user-untracked.txt")).unwrap(),
            "user evidence"
        );
        assert_eq!(report["attribution"], "not_exclusive");
        assert_eq!(report["baseline_git"]["state"], "partial");
        assert_eq!(report["current_git"]["state"], "partial");
        assert_eq!(report["partial"], true);
        let files = report["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        let diff = files
            .iter()
            .find(|file| file["path"] == "main.txt")
            .unwrap()["diff"]
            .as_str()
            .unwrap();
        assert!(diff.contains("+agent addition\n"));
        assert!(!diff.contains("+user staged\n"));
        assert!(!diff.contains("-original\n"));
    }

    #[cfg(unix)]
    #[test]
    fn hostile_git_clean_and_process_filters_never_run_or_receive_credentials() {
        for kind in ["clean", "process"] {
            let root = Temp::new();
            let outside = Temp::new();
            git(&root.0, &["init", "-q"]);
            root.write("tracked.txt", "original\n");
            git(&root.0, &["add", "tracked.txt"]);
            let index = std::fs::read(root.0.join(".git/index")).unwrap();
            root.write(".gitattributes", "tracked.txt filter=hostile\n");
            let quote =
                |path: PathBuf| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
            let filter = format!(
                "printf ran > {}; printf '%s' \"$HARNESS_API_KEY\" > {}; cat",
                quote(outside.0.join("executed")),
                quote(outside.0.join("credential-leak")),
            );
            git(
                &root.0,
                &["config", &format!("filter.hostile.{kind}"), &filter],
            );
            git(&root.0, &["config", "filter.hostile.required", "true"]);
            let config = std::fs::read(root.0.join(".git/config")).unwrap();
            root.write("tracked.txt", "changed before capture\n");
            let baseline = capture(&root.0, &["fixture-not-inherited".into()]).unwrap();
            root.write("tracked.txt", "changed after capture\n");
            let report = finish(&root.0, baseline, &[]).unwrap();
            assert!(
                !outside.0.join("executed").exists(),
                "{kind} filter executed"
            );
            assert!(!outside.0.join("credential-leak").exists());
            assert_eq!(std::fs::read(root.0.join(".git/index")).unwrap(), index);
            assert_eq!(std::fs::read(root.0.join(".git/config")).unwrap(), config);
            assert_eq!(report["baseline_git"]["state"], "partial");
            assert_eq!(
                report["current_git"]["worktree_comparison"],
                "raw_bytes_only"
            );
            assert_eq!(report["files"][0]["path"], "tracked.txt");
            assert!(report["files"][0]["diff"]
                .as_str()
                .unwrap()
                .contains("+changed after capture"));
        }
    }

    #[test]
    fn non_git_rename_delete_binary_unicode_large_and_sensitive_files_are_explicit() {
        let root = Temp::new();
        root.write("旧名字.txt", "rename content\n");
        root.write("delete.txt", "remove this\n");
        root.write("binary.bin", [0, 1, 2]);
        root.write("large.txt", vec![b'a'; MAX_FILE + 1]);
        root.write(".env", "DO_NOT_EXPOSE_hidden");
        root.write("key.pem", "DO_NOT_EXPOSE_key");
        root.write(
            "innocent.txt",
            "-----BEGIN EC PRIVATE KEY-----\nDO_NOT_EXPOSE_ec\n-----END EC PRIVATE KEY-----",
        );
        root.write("credentials.json", "DO_NOT_EXPOSE_credentials");
        root.write("normal.txt", "known-secret-in-normal-file");
        let secrets = vec!["known-secret-in-normal-file".to_owned()];
        let baseline = capture(&root.0, &secrets).unwrap();
        std::fs::rename(root.0.join("旧名字.txt"), root.0.join("新名字.txt")).unwrap();
        std::fs::remove_file(root.0.join("delete.txt")).unwrap();
        root.write("binary.bin", [0, 3, 4]);
        root.write("normal.txt", "now harmless but still excluded");
        let report = finish(&root.0, baseline, &[]).unwrap();
        assert_eq!(report["current_git"]["state"], "not_git");
        let files = report["files"].as_array().unwrap();
        assert!(files.iter().any(|file| file["status"] == "renamed"
            && file["previous_path"] == "旧名字.txt"
            && file["path"] == "新名字.txt"));
        assert!(files
            .iter()
            .any(|file| file["status"] == "deleted" && file["path"] == "delete.txt"));
        assert!(files.iter().any(|file| file["path"] == "binary.bin"
            && file["diff_state"] == "binary_not_shown"
            && file.get("diff").is_none()));
        assert!(report["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["path"] == "large.txt" && file["reason"] == "file_too_large"));
        let wire = report.to_string();
        for hidden in [
            "DO_NOT_EXPOSE",
            "known-secret",
            "normal.txt",
            ".env",
            "key.pem",
        ] {
            assert!(!wire.contains(hidden), "unexpected sensitive output");
        }
    }

    #[test]
    fn secrets_discovered_at_finish_filter_before_images_and_deleted_paths() {
        let root = Temp::new();
        root.write("ordinary.txt", "late-secret-value");
        root.write("late-secret-value.txt", "nonsecret text");
        let baseline = capture(&root.0, &[]).unwrap();
        std::fs::remove_file(root.0.join("ordinary.txt")).unwrap();
        std::fs::remove_file(root.0.join("late-secret-value.txt")).unwrap();
        let report = finish(&root.0, baseline, &["late-secret-value".into()]).unwrap();
        assert!(!report.to_string().contains("late-secret-value"));
        assert!(!report.to_string().contains("ordinary.txt"));
        assert_eq!(report["sensitive_file_count"], 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_to_external_or_hidden_targets_are_never_followed() {
        use std::os::unix::fs::symlink;
        let root = Temp::new();
        let outside = Temp::new();
        outside.write("sensitive.txt", "OUTSIDE_UNIQUE_VALUE");
        root.write(".env", "HIDDEN_UNIQUE_VALUE");
        symlink(outside.0.join("sensitive.txt"), root.0.join("outside-link")).unwrap();
        symlink(&outside.0, root.0.join("outside-directory")).unwrap();
        symlink(root.0.join(".env"), root.0.join("inside-link")).unwrap();
        let baseline = capture(&root.0, &[]).unwrap();
        let report = finish(&root.0, baseline, &[]).unwrap();
        assert_eq!(report["skipped"].as_array().unwrap().len(), 3);
        assert!(!report.to_string().contains("UNIQUE_VALUE"));
        assert_eq!(
            std::fs::read_to_string(outside.0.join("sensitive.txt")).unwrap(),
            "OUTSIDE_UNIQUE_VALUE"
        );
    }

    #[test]
    fn diff_handles_missing_newlines_and_has_a_hard_patch_budget() {
        let diff = unified_diff("中文.txt", Some("before"), Some("after\n")).unwrap();
        assert!(diff.contains("-before\n\\ No newline at end of file\n+after\n"));
        assert_eq!(
            unified_diff("same", Some("x"), Some("x")),
            Some(String::new())
        );
        assert!(unified_diff(
            "large",
            Some(&"a".repeat(MAX_PATCH)),
            Some(&"b".repeat(MAX_PATCH))
        )
        .is_none());
        assert!(unified_diff("new", None, Some("added\n"))
            .unwrap()
            .contains("@@ -0,0 +1,1 @@"));
    }

    #[cfg(unix)]
    #[test]
    fn executable_bit_change_is_visible_without_rewriting_content() {
        use std::os::unix::fs::PermissionsExt;
        let root = Temp::new();
        root.write("script.sh", "echo safe\n");
        std::fs::set_permissions(
            root.0.join("script.sh"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let baseline = capture(&root.0, &[]).unwrap();
        std::fs::set_permissions(
            root.0.join("script.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let report = finish(&root.0, baseline, &[]).unwrap();
        assert_eq!(report["files"][0]["status"], "modified");
        assert_eq!(report["files"][0]["before_mode"], 0o644);
        assert_eq!(report["files"][0]["after_mode"], 0o755);
        assert_eq!(report["files"][0]["diff"], "");
    }

    #[test]
    fn capture_budget_is_shared_across_files_and_binary_before_images_are_not_stored() {
        let root = Temp::new();
        for index in 0..34 {
            root.write(&format!("file-{index:02}.txt"), vec![b'x'; MAX_FILE]);
        }
        root.write("binary.bin", [0, 1, 2, 3]);
        let baseline = capture(&root.0, &[]).unwrap();
        assert!(baseline.files.bytes <= MAX_CAPTURE + 1);
        assert!(!baseline.files.complete);
        let binary = baseline.files.files.get("binary.bin").unwrap();
        assert!(binary.text.is_none());
        assert!(binary.digest.is_some());
        assert!(baseline
            .files
            .files
            .values()
            .any(|entry| entry.kind == "capture_budget_exceeded"));
    }

    #[test]
    fn partial_scans_do_not_invent_added_or_deleted_files_and_report_is_bounded() {
        let root = Temp::new();
        root.write("file.txt", "before");
        let mut baseline = capture(&root.0, &[]).unwrap();
        baseline.files.complete = false;
        root.write("unseen-at-baseline.txt", "after");
        let report = finish(&root.0, baseline, &[]).unwrap();
        assert!(report["files"].as_array().unwrap().is_empty());
        assert!(report["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|file| file["reason"] == "scan_incomplete"));
        assert_eq!(report["partial"], true);
        let mut report = json!({"files":[{"diff":"x".repeat(MAX_REPORT)}],"skipped":[],"baseline_git":{"entries":[]},"current_git":{"entries":[]},"partial":false});
        enforce_report_budget(&mut report);
        assert!(serde_json::to_vec(&report).unwrap().len() <= MAX_REPORT);
        assert_eq!(report["partial"], true);
    }

    #[test]
    fn git_metadata_reads_loose_and_packed_heads_without_commands_or_conversions() {
        let root = Temp::new();
        git(&root.0, &["init", "-q"]);
        git(&root.0, &["config", "user.name", "Fixture"]);
        git(
            &root.0,
            &["config", "user.email", "fixture@example.invalid"],
        );
        for n in 0..12 {
            root.write(
                "nested/main.txt",
                format!("original {}\n", "x".repeat(1000 + n)),
            );
            root.write("中文.txt", format!("第{n}轮\n"));
            git(&root.0, &["add", "."]);
            git(&root.0, &["commit", "-qm", "fixture"]);
        }
        for packed in [false, true] {
            if packed {
                git(&root.0, &["gc", "--aggressive", "--prune=now"]);
            }
            root.write("中文.txt", "staged\n");
            git(&root.0, &["add", "中文.txt"]);
            root.write("中文.txt", "different raw bytes\r\n");
            root.write("outside-index.txt", "untracked or ignored, unknown\n");
            let state = capture(&root.0, &[]).unwrap().git;
            assert_eq!(state["state"], "partial", "{state}");
            assert_eq!(
                state["head_comparison"], "available",
                "packed={packed}: {state}"
            );
            let changed = state["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["path"] == "中文.txt")
                .unwrap();
            assert_eq!(changed["index"], "M");
            assert_eq!(changed["raw_worktree"], "different_bytes");
            assert_eq!(changed["worktree"], "?");
            let new = state["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["path"] == "outside-index.txt")
                .unwrap();
            assert_eq!(new["raw_worktree"], "not_in_index_ignore_unknown");
            assert_eq!(new["untracked"], false);
        }
    }

    #[test]
    fn index_versions_and_corruption_never_silently_report_clean() {
        let root = Temp::new();
        git(&root.0, &["init", "-q"]);
        root.write("a/中文.txt", "content");
        root.write("a/second.txt", "second");
        git(&root.0, &["add", "."]);
        for version in ["2", "3", "4"] {
            git(
                &root.0,
                &["update-index", &format!("--index-version={version}")],
            );
            let state = capture(&root.0, &[]).unwrap().git;
            assert_eq!(state["state"], "partial", "version={version}: {state}");
        }
        let good = std::fs::read(root.0.join(".git/index")).unwrap();
        for n in 0..good.len() {
            assert!(git::index(&good[..n], &mut git::Budget::new()).is_err());
        }
        let mut corrupt = good;
        corrupt[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        std::fs::write(root.0.join(".git/index"), corrupt).unwrap();
        let state = capture(&root.0, &[]).unwrap().git;
        assert_eq!(state["state"], "unavailable");
        assert!(state["entries"].as_array().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn git_metadata_never_follows_external_directory_or_index_links() {
        use std::os::unix::fs::symlink;
        let root = Temp::new();
        let outside = Temp::new();
        outside.write("index", "OUTSIDE_CREDENTIAL");
        symlink(&outside.0, root.0.join(".git")).unwrap();
        let state = capture(&root.0, &[]).unwrap().git;
        assert_eq!(state["state"], "unavailable");
        assert!(!state.to_string().contains("OUTSIDE_CREDENTIAL"));
        std::fs::remove_file(root.0.join(".git")).unwrap();
        std::fs::create_dir(root.0.join(".git")).unwrap();
        symlink(outside.0.join("index"), root.0.join(".git/index")).unwrap();
        let state = capture(&root.0, &[]).unwrap().git;
        assert_eq!(state["state"], "unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn linked_loose_ref_never_falls_back_to_stale_packed_head() {
        use std::os::unix::fs::symlink;
        let root = Temp::new();
        let outside = Temp::new();
        git(&root.0, &["init", "-q"]);
        git(&root.0, &["config", "user.name", "Fixture"]);
        git(&root.0, &["config", "user.email", "fixture@invalid"]);
        root.write("file.txt", "old");
        git(&root.0, &["add", "."]);
        git(&root.0, &["commit", "-qm", "old"]);
        let reference = git(&root.0, &["symbolic-ref", "HEAD"]);
        git(&root.0, &["pack-refs", "--all", "--prune"]);
        root.write("file.txt", "new");
        git(&root.0, &["add", "."]);
        git(&root.0, &["commit", "-qm", "new"]);
        let loose = root.0.join(".git").join(reference.trim());
        std::fs::remove_file(&loose).unwrap();
        outside.write("ref", "OUTSIDE_VALUE_MUST_NOT_BE_READ");
        symlink(outside.0.join("ref"), &loose).unwrap();
        let state = capture(&root.0, &[]).unwrap().git;
        assert_eq!(state["head_comparison"], "unknown", "{state}");
        assert!(state["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["index"] == "?"));
        assert!(!state.to_string().contains("OUTSIDE_VALUE"));
    }

    #[test]
    fn external_ref_markers_and_unobserved_names_do_not_invent_git_state() {
        let root = Temp::new();
        git(&root.0, &["init", "-q"]);
        root.write("Known.txt", "bytes");
        git(&root.0, &["add", "."]);
        let mut baseline = capture(&root.0, &[]).unwrap();
        baseline.files.files.clear();
        let observed = git::observe(
            &baseline.directory,
            &baseline.files,
            &crate::provider::SecretMatcher::new(&[]),
        );
        assert_eq!(
            observed["entries"][0]["raw_worktree"], "unknown",
            "{observed}"
        );
        for marker in ["commondir", "reftable", "refs/replace"] {
            root.write(
                &format!(".git/{marker}"),
                "external target must not be opened",
            );
            let state = capture(&root.0, &[]).unwrap().git;
            assert_eq!(state["state"], "unavailable", "{marker}: {state}");
            std::fs::remove_file(root.0.join(".git").join(marker)).unwrap();
        }
        root.write(
            ".git/packed-refs",
            format!("{} refs/replace/{}\n", "1".repeat(40), "2".repeat(40)),
        );
        assert_eq!(capture(&root.0, &[]).unwrap().git["state"], "unavailable");
    }
}
