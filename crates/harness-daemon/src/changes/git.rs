//! Non-executing Git metadata observations. No config, hooks, filters, transports,
//! external alternates, or subprocess APIs are used. Unsupported data stays unknown.
//! Formats: https://git-scm.com/docs/gitformat-index and gitformat-pack.
mod objects;
use super::{excluded_path, secret_in_path, Scan};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, File, OpenOptions};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
};

type R<T> = Result<T, &'static str>;
const MAX_META: usize = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const MAX_PATH: usize = 4096;
const MAX_TOTAL: usize = 64 * 1024 * 1024;
type Oid = [u8; 20];
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Entry {
    oid: Oid,
    mode: u32,
    stage: u8,
    flags: u16,
}
pub(super) struct Budget {
    left: usize,
    objects: usize,
}
impl Budget {
    pub(super) fn new() -> Self {
        Self {
            left: MAX_TOTAL,
            objects: 0,
        }
    }
    fn charge(&mut self, bytes: usize) -> R<()> {
        self.left = self
            .left
            .checked_sub(bytes)
            .ok_or("git_metadata_budget_exceeded")?;
        Ok(())
    }
}

pub(super) fn blob_oid(bytes: &[u8]) -> Oid {
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()));
    h.update(bytes);
    h.finalize().into()
}
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH
        && !path.contains(['\\', ':', '\0'])
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}
fn file(dir: &Dir, path: &str) -> R<File> {
    if !safe_path(path) {
        return Err("git_metadata_path_invalid");
    }
    let mut current = dir.try_clone().map_err(|_| "git_metadata_unreadable")?;
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            current = current
                .open_dir_nofollow(part)
                .map_err(|_| "git_metadata_unreadable")?;
        } else {
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No).nonblock(true);
            let opened = current
                .open_with(part, &options)
                .map_err(|_| "git_metadata_unreadable")?;
            if !opened
                .metadata()
                .map_err(|_| "git_metadata_unreadable")?
                .is_file()
            {
                return Err("git_metadata_not_regular");
            }
            return Ok(opened);
        }
    }
    Err("git_metadata_path_invalid")
}
/// Prove absence without following any intermediate or final symlink. Other
/// read errors must never be reinterpreted as a missing loose ref/object.
fn missing(dir: &Dir, path: &str) -> bool {
    if !safe_path(path) {
        return false;
    }
    let Ok(mut current) = dir.try_clone() else {
        return false;
    };
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            return current
                .symlink_metadata(part)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
        }
        match current.open_dir_nofollow(part) {
            Ok(child) => current = child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(_) => return false,
        }
    }
    false
}
fn read(dir: &Dir, path: &str, limit: usize, budget: &mut Budget) -> R<Vec<u8>> {
    let file = file(dir, path)?;
    let before = file.metadata().map_err(|_| "git_metadata_unreadable")?;
    if before.len() > limit as u64 {
        return Err("git_metadata_file_too_large");
    }
    let max = limit.min(budget.left);
    let mut bytes = Vec::new();
    (&file)
        .take((max + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| "git_metadata_unreadable")?;
    budget.charge(bytes.len())?;
    let after = file.metadata().map_err(|_| "git_metadata_unreadable")?;
    if bytes.len() > limit
        || bytes.len() as u64 != before.len()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return Err("git_metadata_changed_or_truncated");
    }
    Ok(bytes)
}
fn take<'a>(data: &'a [u8], at: &mut usize, n: usize) -> R<&'a [u8]> {
    let end = at.checked_add(n).ok_or("git_metadata_invalid")?;
    let value = data.get(*at..end).ok_or("git_metadata_truncated")?;
    *at = end;
    Ok(value)
}
fn u32be(data: &[u8], at: usize) -> R<u32> {
    Ok(u32::from_be_bytes(
        data.get(at..at + 4)
            .ok_or("git_metadata_truncated")?
            .try_into()
            .unwrap(),
    ))
}
fn oid_hex(text: &str) -> R<Oid> {
    if text.len() != 40 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("git_oid_unsupported");
    }
    let mut oid = [0; 20];
    for (i, byte) in oid.iter_mut().enumerate() {
        *byte =
            u8::from_str_radix(&text[2 * i..2 * i + 2], 16).map_err(|_| "git_oid_unsupported")?;
    }
    Ok(oid)
}
fn hex(oid: &Oid) -> String {
    oid.iter().map(|b| format!("{b:02x}")).collect()
}
fn offset_varint(data: &[u8], at: &mut usize) -> R<usize> {
    let mut byte = take(data, at, 1)?[0];
    let mut n = (byte & 127) as usize;
    for _ in 0..9 {
        if byte & 128 == 0 {
            return Ok(n);
        }
        byte = take(data, at, 1)?[0];
        n = n
            .checked_add(1)
            .and_then(|v| v.checked_mul(128))
            .and_then(|v| v.checked_add((byte & 127) as usize))
            .ok_or("git_integer_overflow")?;
    }
    Err("git_integer_overflow")
}
pub(super) fn index(data: &[u8], budget: &mut Budget) -> R<BTreeMap<String, Entry>> {
    if data.len() < 32 || &data[..4] != b"DIRC" {
        return Err("git_index_invalid");
    }
    let version = u32be(data, 4)?;
    if !(2..=4).contains(&version) {
        return Err("git_index_version_unsupported");
    }
    let count = u32be(data, 8)? as usize;
    if count > MAX_ENTRIES {
        return Err("git_index_entry_budget");
    }
    let end = data.len() - 20;
    if Sha1::digest(&data[..end])[..] != data[end..] {
        return Err("git_index_checksum_or_format");
    }
    let data = &data[..end];
    let mut at = 12;
    let mut result = BTreeMap::new();
    let mut previous = Vec::<u8>::new();
    let mut prior_key = None;
    for _ in 0..count {
        let start = at;
        let stat = take(data, &mut at, 40)?;
        let mode = u32be(stat, 24)?;
        if ![0o100644, 0o100755, 0o120000, 0o160000].contains(&mode) {
            return Err("git_index_mode_unsupported");
        }
        let oid = take(data, &mut at, 20)?.try_into().unwrap();
        let flags = u16::from_be_bytes(take(data, &mut at, 2)?.try_into().unwrap());
        let extended = if flags & 0x4000 != 0 {
            if version == 2 {
                return Err("git_index_invalid");
            }
            u16::from_be_bytes(take(data, &mut at, 2)?.try_into().unwrap())
        } else {
            0
        };
        if extended & !0x6000 != 0 {
            return Err("git_index_flags_unsupported");
        }
        let cut = if version == 4 {
            offset_varint(data, &mut at)?
        } else {
            previous.len()
        };
        let keep = previous
            .len()
            .checked_sub(cut)
            .ok_or("git_index_path_invalid")?;
        let tail = data.get(at..).ok_or("git_index_invalid")?;
        let length = tail
            .iter()
            .position(|b| *b == 0)
            .ok_or("git_index_path_invalid")?;
        if keep + length > MAX_PATH {
            return Err("git_index_path_budget");
        }
        let mut name = previous[..keep].to_vec();
        name.extend_from_slice(take(data, &mut at, length)?);
        take(data, &mut at, 1)?;
        if version < 4 {
            let padding = (8 - (at - start) % 8) % 8;
            if take(data, &mut at, padding)?.iter().any(|b| *b != 0) {
                return Err("git_index_padding_invalid");
            }
        }
        let path = std::str::from_utf8(&name).map_err(|_| "git_path_encoding_unsupported")?;
        if !safe_path(path)
            || path.split('/').any(|c| c.eq_ignore_ascii_case(".git"))
            || (flags & 0xfff) != 0xfff && usize::from(flags & 0xfff) != name.len()
        {
            return Err("git_index_path_invalid");
        }
        let stage = ((flags >> 12) & 3) as u8;
        let key = (name.clone(), stage);
        if prior_key.as_ref().is_some_and(|prior| prior >= &key) {
            return Err("git_index_order_invalid");
        }
        prior_key = Some(key);
        budget.charge(path.len() + 64)?;
        // A conflict's stages share a path; retain an explicit conflict marker.
        result.insert(
            path.to_owned(),
            Entry {
                oid,
                mode,
                stage,
                flags: extended | (flags & 0x8000),
            },
        );
        previous = name;
    }
    while at < data.len() {
        let signature = take(data, &mut at, 4)?;
        let size = u32::from_be_bytes(take(data, &mut at, 4)?.try_into().unwrap()) as usize;
        if !signature[0].is_ascii_uppercase() {
            return Err("git_index_extension_unsupported");
        }
        take(data, &mut at, size)?;
    }
    Ok(result)
}
fn valid_ref(name: &str) -> bool {
    safe_path(name)
        && name.starts_with("refs/")
        && !name.contains("..")
        && !name.contains("@{")
        && !name.ends_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "~^?*[".contains(c))
        && name
            .split('/')
            .all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}
fn packed_refs(data: &[u8], budget: &mut Budget) -> R<BTreeMap<String, Oid>> {
    let text = std::str::from_utf8(data).map_err(|_| "git_ref_invalid")?;
    let mut result = BTreeMap::new();
    let mut can_peel = false;
    for (i, line) in text.lines().enumerate() {
        if i >= MAX_ENTRIES {
            return Err("git_ref_count_budget");
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(oid) = line.strip_prefix('^') {
            if !can_peel {
                return Err("git_ref_invalid");
            }
            oid_hex(oid)?;
            can_peel = false;
            continue;
        }
        let (oid, name) = line.split_once(' ').ok_or("git_ref_invalid")?;
        if !valid_ref(name) {
            return Err("git_ref_invalid");
        }
        if name.starts_with("refs/replace/") {
            return Err("git_replaced_metadata_unsupported");
        }
        budget.charge(name.len() + 64)?;
        if result.insert(name.to_owned(), oid_hex(oid)?).is_some() {
            return Err("git_ref_duplicate");
        }
        can_peel = true;
    }
    Ok(result)
}
fn head(dir: &Dir, budget: &mut Budget) -> R<Option<Oid>> {
    let mut name = "HEAD".to_owned();
    let mut seen = BTreeSet::new();
    for _ in 0..5 {
        if !seen.insert(name.clone()) {
            return Err("git_ref_cycle");
        }
        let bytes = match read(dir, &name, 8192, budget) {
            Ok(bytes) => bytes,
            Err(_) if name.starts_with("refs/") && missing(dir, &name) => {
                let packed = match read(dir, "packed-refs", MAX_META, budget) {
                    Ok(v) => v,
                    Err(_) if missing(dir, "packed-refs") => return Ok(None),
                    Err(reason) => return Err(reason),
                };
                return Ok(packed_refs(&packed, budget)?.get(&name).copied());
            }
            Err(e) => return Err(e),
        };
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| "git_ref_invalid")?
            .trim();
        if let Some(reference) = text.strip_prefix("ref: ") {
            if !valid_ref(reference) {
                return Err("git_ref_invalid");
            }
            name = reference.to_owned();
        } else {
            return oid_hex(text).map(Some);
        }
    }
    Err("git_ref_depth_exceeded")
}

pub(super) fn observe(root: &Dir, scan: &Scan, secrets: &crate::provider::SecretMatcher) -> Value {
    match observe_inner(root, scan, secrets) {
        Ok(value) => value,
        Err(reason) => {
            json!({"state":"unavailable","entries":[],"reason":reason,"scope":"bounded_git_metadata_not_task_attribution"})
        }
    }
}
fn observe_inner(root: &Dir, scan: &Scan, secrets: &crate::provider::SecretMatcher) -> R<Value> {
    match root.symlink_metadata(".git") {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"state":"not_git","entries":[],"scope":"project_root_metadata_only"}))
        }
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        _ => return Err("git_external_or_linked_metadata_unsupported"),
    }
    let dir = root
        .open_dir_nofollow(".git")
        .map_err(|_| "git_metadata_unreadable")?;
    let mut budget = Budget::new();
    for marker in ["commondir", "reftable", "refs/replace"] {
        if !missing(&dir, marker) {
            return Err("git_external_or_replaced_metadata_unsupported");
        }
    }
    if !missing(&dir, "packed-refs") {
        let packed = read(&dir, "packed-refs", MAX_META, &mut budget)?;
        packed_refs(&packed, &mut budget)?;
    }
    let index_bytes = read(&dir, "index", MAX_META, &mut budget)?;
    let entries = index(&index_bytes, &mut budget)?;
    let mut reasons = vec!["worktree_conversion_and_ignore_rules_not_executed"];
    let head_entries = match head(&dir, &mut budget).and_then(|oid| match oid {
        Some(oid) => objects::tree(&dir, oid, &mut budget),
        None => Ok(BTreeMap::new()),
    }) {
        Ok(v) => Some(v),
        Err(reason) => {
            reasons.push(reason);
            None
        }
    };
    let mut paths: BTreeSet<_> = entries.keys().cloned().collect();
    if let Some(head) = &head_entries {
        paths.extend(head.keys().cloned());
    }
    paths.extend(scan.files.keys().cloned());
    let mut out = Vec::new();
    let mut omitted = 0;
    for path in paths {
        if excluded_path(&path) || scan.sensitive.contains(&path) || secret_in_path(&path, secrets)
        {
            omitted += 1;
            continue;
        }
        let item = entries.get(&path);
        let prior = head_entries.as_ref().and_then(|h| h.get(&path));
        let staged = if item.is_some_and(|e| e.stage != 0) {
            "U"
        } else if item.is_some_and(|e| e.flags & 0x2000 != 0) || head_entries.is_none() {
            "?"
        } else {
            match (prior, item) {
                (None, Some(_)) => "A",
                (Some(_), None) => "D",
                (Some(a), Some(b)) if a.oid != b.oid || a.mode != b.mode => "M",
                _ => " ",
            }
        };
        let raw = match (item, scan.files.get(&path)) {
            (Some(e), Some(_f))
                if e.stage != 0 || e.flags != 0 || ![0o100644, 0o100755].contains(&e.mode) =>
            {
                "unknown"
            }
            (Some(e), Some(f)) => match f.git_oid {
                Some(oid) if oid == e.oid => "equal_bytes",
                Some(_) => "different_bytes",
                None => "unknown",
            },
            (Some(e), None)
                if scan.complete
                    && e.stage == 0
                    && e.flags == 0
                    && [0o100644, 0o100755].contains(&e.mode)
                    && missing(root, &path) =>
            {
                "missing"
            }
            (Some(_), None) => "unknown",
            (None, Some(_)) => "not_in_index_ignore_unknown",
            _ => "not_present",
        };
        if staged == " " && raw == "equal_bytes" {
            continue;
        }
        if out.len() >= super::MAX_REPORT_FILES {
            omitted += 1;
            continue;
        }
        out.push(
            json!({"path":path,"index":staged,"worktree":"?","untracked":false,"raw_worktree":raw}),
        );
    }
    Ok(
        json!({"state":"partial","entries":out,"omitted_count":omitted,"reasons":reasons,"scope":"bounded_git_metadata_not_task_attribution","head_comparison":if head_entries.is_some(){"available"}else{"unknown"},"worktree_comparison":"raw_bytes_only","limits":{"metadata_bytes":MAX_TOTAL,"entries":MAX_ENTRIES,"object_bytes":objects::MAX_OBJECT,"delta_depth":objects::MAX_DELTA_DEPTH},"notice":"暂存列比较 HEAD 与 index；工作区仅比较原始字节，未执行过滤器、编码转换或忽略规则，不能据此认定 Git 干净。"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packed_refs_require_complete_valid_traversal_before_unborn_result() {
        let mut budget = Budget::new();
        assert!(packed_refs(b"not-an-oid refs/heads/main\n", &mut budget).is_err());
        assert!(packed_refs(
            format!("{} refs/heads/invalid name\n", "1".repeat(40)).as_bytes(),
            &mut budget
        )
        .is_err());
        assert!(packed_refs(
            format!(
                "{} refs/heads/main\n{} refs/heads/main\n",
                "1".repeat(40),
                "2".repeat(40)
            )
            .as_bytes(),
            &mut budget
        )
        .is_err());
        let lines = (0..=MAX_ENTRIES)
            .map(|i| format!("{} refs/heads/branch-{i}\n", "1".repeat(40)))
            .collect::<String>();
        assert!(packed_refs(lines.as_bytes(), &mut Budget::new()).is_err());
        assert!(
            packed_refs(b"# pack-refs with: peeled sorted\n", &mut budget)
                .unwrap()
                .is_empty()
        );
    }
}
