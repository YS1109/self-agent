//! Native file capabilities: pin the selected directory once, reject symlinks
//! in every traversed component, and never reopen checked ambient paths.
use anyhow::{bail, Context, Result};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::{Dir, OpenOptions};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

const MAX_FILE: usize = 256 * 1024;
const MAX_NODES: usize = 2000;
const MAX_DEPTH: usize = 24;
const MAX_SCAN_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESULT: usize = 64 * 1024;
const MAX_MATCHES: usize = 100;

pub(crate) struct ScopedProject {
    root: Dir,
    #[cfg(test)]
    before_rename: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl ScopedProject {
    /// The caller supplies its selected absolute project root. No canonicalize
    /// followed by ambient open: walk from the filesystem root without links.
    pub(crate) fn open(root: &Path) -> Result<Self> {
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
                        bail!("项目文件工具不接受远程或设备根目录");
                    }
                    anchor.push(prefix.as_os_str());
                }
                Component::RootDir => anchor.push(component.as_os_str()),
                Component::Normal(name) => names.push(name.to_os_string()),
                Component::CurDir => {}
                Component::ParentDir => bail!("项目根目录不能包含上级路径"),
            }
        }
        let mut root = Dir::open_ambient_dir(anchor, cap_std::ambient_authority())
            .context("无法打开项目所在文件系统")?;
        for name in names {
            root = root
                .open_dir_nofollow(name)
                .context("项目目录不可用或包含符号链接")?;
        }
        Ok(Self {
            root,
            #[cfg(test)]
            before_rename: None,
        })
    }

    pub(crate) fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            root: self.root.try_clone().context("无法复制项目目录能力")?,
            #[cfg(test)]
            before_rename: self.before_rename.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_before_rename_hook(&mut self, hook: std::sync::Arc<dyn Fn() + Send + Sync>) {
        self.before_rename = Some(hook);
    }

    fn parent(&self, relative: &str) -> Result<(Dir, OsString)> {
        let mut components = relative_components(relative)?;
        let name = components.pop().context("文件路径不能指向项目目录本身")?;
        let mut directory = self.root.try_clone().context("项目目录能力不可用")?;
        for component in components {
            directory = directory
                .open_dir_nofollow(component)
                .context("父目录不存在、不是目录或包含符号链接")?;
        }
        Ok((directory, name))
    }

    pub(crate) fn read(&self, relative: &str) -> Result<String> {
        let (parent, name) = self.parent(relative)?;
        read_text(&parent, &name, &mut (MAX_FILE + 1))
    }

    /// Replace the directory entry atomically, rather than truncating an
    /// existing inode (which might also be hard-linked outside the project).
    pub(crate) fn write(&self, relative: &str, content: &str) -> Result<usize> {
        if content.len() > MAX_FILE {
            bail!("写入内容超过 256 KiB 限制");
        }
        let (parent, name) = self.parent(relative)?;
        let permissions = match parent.symlink_metadata(&name) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    bail!("只能替换项目内普通文件，不能写入符号链接或特殊文件");
                }
                if metadata.permissions().readonly() {
                    bail!("不能覆盖只读文件");
                }
                Some(metadata.permissions())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => bail!("无法检查待写入文件"),
        };
        let temporary = format!(".self-agent-write-{}.tmp", harness_core::new_id());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No)
            .nonblock(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = parent
            .open_with(&temporary, &options)
            .context("无法创建项目内临时文件")?;
        let result = (|| {
            if !file.metadata()?.is_file() {
                bail!("临时写入目标不是普通文件");
            }
            file.write_all(content.as_bytes())
                .context("无法写入项目文件")?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions)?;
            }
            file.sync_all().context("无法保存项目文件内容")?;
            drop(file);
            #[cfg(test)]
            if let Some(hook) = &self.before_rename {
                hook();
            }
            parent
                .rename(&temporary, &parent, &name)
                .context("无法替换项目文件")?;
            Ok(content.len())
        })();
        if result.is_err() {
            let _ = parent.remove_file(&temporary);
        }
        result
    }

    /// Complete matching source lines are redacted before character or encoded
    /// result limits. Names are redacted too; skipped paths are never echoed.
    pub(crate) fn scan(
        &self,
        relative: &str,
        query: Option<&str>,
        known_secrets: &[String],
    ) -> Result<Value> {
        if query.is_some_and(|query| query.len() > 16_384) {
            bail!("搜索文本超过 16384 字节限制");
        }
        if known_secrets.iter().map(String::len).sum::<usize>() > 512 * 1024
            || known_secrets.len() > 256
        {
            bail!("敏感值配置超过文件搜索预算");
        }
        let components = relative_components(relative)?;
        if components.iter().any(|name| excluded(name)) {
            bail!("该生成或版本控制目录不参与文件搜索");
        }
        let mut scan = Scan {
            remaining: MAX_SCAN_BYTES,
            ..Scan::default()
        };
        if components.is_empty() {
            scan.directory(&self.root, "", 0, query, known_secrets)?;
        } else {
            let (parent, name) = self.parent(relative)?;
            let prefix = components
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            scan.entry(
                &parent,
                &name,
                &prefix,
                components.len(),
                query,
                known_secrets,
            )?;
        }
        Ok(if query.is_some() {
            json!({"matches":scan.values,"truncated":scan.partial,"partial":scan.partial,"skipped":scan.skipped})
        } else {
            json!({"files":scan.values,"truncated":scan.partial,"partial":scan.partial,"skipped":scan.skipped})
        })
    }
}

fn relative_components(relative: &str) -> Result<Vec<OsString>> {
    if relative.len() > 4096 || relative.contains('\0') {
        bail!("项目相对路径无效或超过 4096 字节");
    }
    let mut names = Vec::new();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(name) => {
                // Refuse Windows stream/device syntax on every platform, so a
                // saved task cannot gain new path semantics after migration.
                if name.to_string_lossy().contains([':', '\\']) {
                    bail!("文件路径包含不支持的分隔符或设备语法");
                }
                names.push(name.to_os_string());
            }
            Component::CurDir => {}
            _ => bail!("请使用项目内相对路径，不能包含上级目录或绝对路径"),
        }
    }
    if names.len() > MAX_DEPTH {
        bail!("项目路径超过 24 层限制");
    }
    Ok(names)
}

fn read_text(directory: &Dir, name: &OsString, remaining: &mut usize) -> Result<String> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).nonblock(true);
    let file = directory
        .open_with(name, &options)
        .context("无法打开项目普通文件，符号链接不会被跟随")?;
    let metadata = file.metadata().context("无法检查已打开文件")?;
    if !metadata.is_file() || metadata.len() > MAX_FILE as u64 {
        bail!("只能读取不超过 256 KiB 的普通文件");
    }
    if metadata.len() as usize > *remaining {
        bail!("文件搜索累计读取预算不足");
    }
    let limit = (*remaining).min(MAX_FILE + 1);
    let mut bytes = Vec::new();
    let outcome = (&file).take(limit as u64).read_to_end(&mut bytes);
    *remaining = remaining.saturating_sub(bytes.len());
    outcome.context("无法读取项目文件")?;
    let after = file.metadata().context("无法复核已打开文件")?;
    if bytes.len() > MAX_FILE
        || bytes.len() as u64 != metadata.len()
        || after.len() != metadata.len()
        || after.modified().ok() != metadata.modified().ok()
    {
        bail!("文件在读取期间变化或超过读取限制，请重新检查");
    }
    String::from_utf8(bytes).context("文件不是 UTF-8 文本")
}

fn excluded(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | "node_modules" | "target" | ".venv")
    )
}

fn redact(text: &str, secrets: &[String]) -> String {
    let mut secrets: Vec<_> = secrets.iter().filter(|secret| !secret.is_empty()).collect();
    secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    secrets.dedup();
    let mut safe = String::with_capacity(text.len());
    let mut remaining = text;
    while !remaining.is_empty() {
        if let Some(secret) = secrets
            .iter()
            .find(|secret| remaining.starts_with(secret.as_str()))
        {
            // Match only original text: never reprocess replacement markers,
            // even if a short credential is itself part of that marker.
            safe.push_str("[凭证已隐藏]");
            for _ in 0..secret.matches('\n').count() {
                safe.push('\n');
            }
            remaining = &remaining[secret.len()..];
        } else {
            let character = remaining.chars().next().unwrap();
            safe.push(character);
            remaining = &remaining[character.len_utf8()..];
        }
    }
    safe
}

#[derive(Default)]
struct Scan {
    values: Vec<Value>,
    visited: usize,
    remaining: usize,
    encoded: usize,
    partial: bool,
    skipped: usize,
}

impl Scan {
    fn push(&mut self, value: Value) -> Result<bool> {
        let length = serde_json::to_vec(&value)?.len();
        if self.encoded + length + 1 > MAX_RESULT - 256 {
            self.partial = true;
            return Ok(false);
        }
        self.encoded += length + 1;
        self.values.push(value);
        Ok(true)
    }

    fn directory(
        &mut self,
        dir: &Dir,
        prefix: &str,
        depth: usize,
        query: Option<&str>,
        secrets: &[String],
    ) -> Result<()> {
        if depth >= MAX_DEPTH || self.visited >= MAX_NODES {
            self.partial = true;
            return Ok(());
        }
        let mut names = Vec::new();
        for entry in dir.entries().context("无法列出项目目录")? {
            if self.visited >= MAX_NODES {
                self.partial = true;
                break;
            }
            self.visited += 1;
            match entry {
                Ok(entry) => names.push(entry.file_name()),
                Err(_) => {
                    self.partial = true;
                    self.skipped += 1;
                }
            }
        }
        names.sort();
        for name in names {
            if query.is_some() && self.values.len() >= MAX_MATCHES {
                self.partial = true;
                break;
            }
            if excluded(&name) {
                continue;
            }
            let Some(text) = name.to_str() else {
                self.partial = true;
                self.skipped += 1;
                continue;
            };
            let path = if prefix.is_empty() {
                text.into()
            } else {
                format!("{prefix}/{text}")
            };
            if path.len() > 4096 {
                self.partial = true;
                self.skipped += 1;
                continue;
            }
            if self
                .entry(dir, &name, &path, depth + 1, query, secrets)
                .is_err()
            {
                self.partial = true;
                self.skipped += 1;
            }
            if self.encoded >= MAX_RESULT - 8192 {
                self.partial = true;
                break;
            }
        }
        Ok(())
    }

    fn entry(
        &mut self,
        parent: &Dir,
        name: &OsString,
        path: &str,
        depth: usize,
        query: Option<&str>,
        secrets: &[String],
    ) -> Result<()> {
        let metadata = parent
            .symlink_metadata(name)
            .context("项目搜索路径不可用")?;
        if metadata.file_type().is_symlink() {
            self.skipped += 1;
            self.partial = true;
            return Ok(());
        }
        if metadata.is_dir() {
            let dir = parent
                .open_dir_nofollow(name)
                .context("项目目录已变化或包含符号链接")?;
            return self.directory(&dir, path, depth, query, secrets);
        }
        if !metadata.is_file() {
            self.skipped += 1;
            self.partial = true;
            return Ok(());
        }
        let safe_path = redact(path, secrets);
        if let Some(query) = query {
            let content = read_text(parent, name, &mut self.remaining)?;
            let safe = redact(&content, secrets);
            let safe_lines: Vec<_> = safe.lines().collect();
            for (line, source) in content.lines().enumerate() {
                if source.contains(query) {
                    let text = safe_lines
                        .get(line)
                        .copied()
                        .unwrap_or("[内容已隐藏]")
                        .chars()
                        .take(500)
                        .collect::<String>();
                    if !self.push(json!({"path":safe_path,"line":line+1,"text":text}))? {
                        break;
                    }
                    if self.values.len() >= MAX_MATCHES {
                        self.partial = true;
                        break;
                    }
                }
            }
        } else {
            self.push(json!(safe_path))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("self-agent-files-{}", harness_core::new_id()));
            std::fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
        fn open(&self) -> ScopedProject {
            ScopedProject::open(&self.0).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn before_rename_hook_is_shared_by_cloned_capability() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let root = std::env::temp_dir().join(format!("self-agent-hook-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let mut scoped = ScopedProject::open(&root).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let checked_root = root.clone();
        scoped.set_before_rename_hook(Arc::new(move || {
            assert!(!checked_root.join("new.txt").exists());
            observed.fetch_add(1, Ordering::SeqCst);
        }));
        scoped
            .try_clone()
            .unwrap()
            .write("new.txt", "done")
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(scoped.read("new.txt").unwrap(), "done");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn normal_files_and_existing_parent_only() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("目录")).unwrap();
        let project = fixture.open();
        assert_eq!(project.write("目录/文件.txt", "你好").unwrap(), 6);
        assert_eq!(
            project.try_clone().unwrap().read("目录/文件.txt").unwrap(),
            "你好"
        );
        assert!(project.write("missing/file", "x").is_err());
        assert!(project.read("missing").is_err());
        assert!(project.scan("missing", None, &[]).is_err());
        assert!(project.read("目录").is_err());
        assert!(project.write("目录", "x").is_err());
        for path in [
            "../outside",
            "/etc/passwd",
            "a/../../outside",
            "C:/outside",
            "a\0b",
        ] {
            assert!(project.read(path).is_err(), "{path}");
            assert!(project.write(path, "x").is_err(), "{path}");
        }
        assert!(project.write(".", "x").is_err());
        let listed = project.scan(".", None, &[]).unwrap();
        assert_eq!(listed["files"], json!(["目录/文件.txt"]));
        assert_eq!(listed["partial"], false);
    }

    #[test]
    fn file_size_utf8_and_scan_limits_are_real() {
        let fixture = Fixture::new();
        let project = fixture.open();
        assert!(project.write("big", &"a".repeat(MAX_FILE + 1)).is_err());
        assert!(!fixture.0.join("big").exists());
        std::fs::write(fixture.0.join("big"), vec![b'x'; MAX_FILE + 1]).unwrap();
        assert!(project.read("big").is_err());
        std::fs::write(fixture.0.join("binary"), [255]).unwrap();
        assert!(project.read("binary").is_err());
        for index in 0..120 {
            std::fs::write(
                fixture.0.join(format!("match-{index:03}")),
                "find 中文".repeat(200),
            )
            .unwrap();
        }
        let matches = project.scan(".", Some("find"), &[]).unwrap();
        assert!(serde_json::to_vec(&matches).unwrap().len() <= MAX_RESULT);
        assert!(matches["matches"].as_array().unwrap().len() <= MAX_MATCHES);
        assert_eq!(matches["partial"], true);
        assert!(matches["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["text"].as_str().unwrap().chars().count() <= 500));
    }

    #[test]
    fn scan_redacts_before_line_truncation_and_preserves_line_numbers() {
        let fixture = Fixture::new();
        let project = fixture.open();
        let secret = "fixture-search-boundary-secret";
        std::fs::write(
            fixture.0.join("source"),
            format!("{}{}TAIL", "中".repeat(492), secret),
        )
        .unwrap();
        let found = project.scan(".", Some(secret), &[secret.into()]).unwrap();
        let text = found["matches"][0]["text"].as_str().unwrap();
        assert!(!text.contains("fixture-"));
        assert!(text.contains("[凭证已隐藏]"));
        std::fs::write(
            fixture.0.join("multiline"),
            "first\nTOP_SECRET\nBOTTOM_SECRET tail\nlast",
        )
        .unwrap();
        let found = project
            .scan(
                "multiline",
                Some("BOTTOM_SECRET"),
                &["TOP_SECRET\nBOTTOM_SECRET".into()],
            )
            .unwrap();
        assert_eq!(found["matches"][0]["line"], 3);
        assert!(!found.to_string().contains("BOTTOM_SECRET"));
        assert!(!found.to_string().contains("TOP_SECRET"));
    }

    #[test]
    fn excluded_directories_depth_and_node_count_are_bounded() {
        let fixture = Fixture::new();
        for directory in [".git", "node_modules", "target", ".venv"] {
            std::fs::create_dir(fixture.0.join(directory)).unwrap();
            std::fs::write(fixture.0.join(directory).join("hidden"), "hidden").unwrap();
        }
        let project = fixture.open();
        assert_eq!(project.scan(".", None, &[]).unwrap()["files"], json!([]));
        assert!(project.scan(".git", None, &[]).is_err());
        let mut path = fixture.0.clone();
        for _ in 0..MAX_DEPTH + 1 {
            path.push("nested");
            std::fs::create_dir(&path).unwrap();
        }
        std::fs::write(path.join("deep"), "deep").unwrap();
        assert_eq!(project.scan(".", None, &[]).unwrap()["partial"], true);
        assert!(project
            .read(&format!("{}deep", "nested/".repeat(MAX_DEPTH + 1)))
            .is_err());
        for index in 0..MAX_NODES + 1 {
            std::fs::write(fixture.0.join(format!("f{index:04}")), "").unwrap();
        }
        let listed = project.scan(".", None, &[]).unwrap();
        assert_eq!(listed["partial"], true);
        assert!(listed["files"].as_array().unwrap().len() <= MAX_NODES);
    }

    #[test]
    fn readonly_target_and_write_errors_leave_no_temp_files() {
        let fixture = Fixture::new();
        let path = fixture.0.join("readonly");
        std::fs::write(&path, "original").unwrap();
        let original_permissions = std::fs::metadata(&path).unwrap().permissions();
        let mut permissions = original_permissions.clone();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();
        let project = fixture.open();
        assert!(project.write("readonly", "replacement").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        assert!(std::fs::read_dir(&fixture.0).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".self-agent-write-")));
        #[cfg(windows)]
        {
            std::fs::set_permissions(path, original_permissions).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn links_and_special_files_never_escape_or_block() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let outside = Fixture::new();
        std::fs::write(outside.0.join("sentinel"), "outside").unwrap();
        symlink(outside.0.join("sentinel"), fixture.0.join("link")).unwrap();
        symlink(&outside.0, fixture.0.join("parent_link")).unwrap();
        let project = fixture.open();
        assert!(project.read("link").is_err());
        assert!(project.write("link", "changed").is_err());
        assert!(project.read("parent_link/sentinel").is_err());
        assert!(project.write("parent_link/sentinel", "changed").is_err());
        assert_eq!(
            std::fs::read_to_string(outside.0.join("sentinel")).unwrap(),
            "outside"
        );
        assert!(ScopedProject::open(&fixture.0.join("parent_link")).is_err());
        let fifo = fixture.0.join("fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        assert!(project.read("fifo").is_err());
        assert!(project.write("fifo", "x").is_err());
        let socket = fixture.0.join("socket");
        let _listener = match std::os::unix::net::UnixListener::bind(&socket) {
            Ok(listener) => {
                assert!(project.read("socket").is_err());
                assert!(project.write("socket", "x").is_err());
                Some(listener)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ) =>
            {
                eprintln!("SKIP Unix socket fixture: execution environment rejects socket creation ({error}); FIFO and symlink checks still ran");
                None
            }
            Err(error) => panic!("Cannot create Unix socket fixture: {error}"),
        };
        let scan = project.scan(".", None, &[]).unwrap();
        assert_eq!(scan["files"], json!([]));
        assert_eq!(scan["partial"], true);
    }

    #[cfg(unix)]
    #[test]
    fn pinned_root_and_atomic_replace_do_not_follow_replaced_links_or_mutate_hardlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let root = fixture.0.join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("local"), "local").unwrap();
        std::fs::write(outside.0.join("local"), "outside").unwrap();
        let project = ScopedProject::open(&root).unwrap();
        std::fs::rename(&root, fixture.0.join("original")).unwrap();
        symlink(&outside.0, &root).unwrap();
        assert!(ScopedProject::open(&root).is_err());
        assert_eq!(project.read("local").unwrap(), "local");
        project.write("local", "replacement").unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.0.join("local")).unwrap(),
            "outside"
        );
        let target = fixture.0.join("original/local");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o751)).unwrap();
        std::fs::hard_link(&target, outside.0.join("hardlink")).unwrap();
        project.write("local", "new-inode").unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.0.join("hardlink")).unwrap(),
            "replacement"
        );
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[test]
    fn redaction_never_reprocesses_its_own_marker() {
        let secrets = vec!["[".into(); 256];
        assert_eq!(redact("[", &secrets), "[凭证已隐藏]");
        assert_eq!(
            redact("abcdef", &["abc".into(), "abcdef".into()]),
            "[凭证已隐藏]"
        );
    }
}
