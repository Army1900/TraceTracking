use anyhow::{Context, Result, bail};
use notify::{EventKind, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

fn run(root: &str, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("无法启动 Git")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim())
    }
    Ok(String::from_utf8(out.stdout)?)
}
pub fn history(root: &str, path: &str) -> Result<Vec<String>> {
    Ok(run(
        root,
        &[
            "--literal-pathspecs",
            "log",
            "-100",
            "--format=%H %as %an · %s",
            "--",
            path,
        ],
    )?
    .lines()
    .map(str::to_owned)
    .collect())
}
pub fn body(root: &str, path: &str, rev: &str) -> Result<String> {
    let prefix = run(root, &["rev-parse", "--show-prefix"])?;
    let object = format!("{rev}:{}{path}", prefix.trim_end());
    let size = run(root, &["cat-file", "-s", &object])?
        .trim()
        .parse::<u64>()?;
    if size > crate::store::LIMIT {
        bail!("Git 版本超过正文大小限制")
    }
    run(root, &["cat-file", "blob", &object])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStatus(pub [u8; 2]);

impl FileStatus {
    pub fn conflicted(self) -> bool {
        matches!(
            &self.0,
            b"DD" | b"AU" | b"UD" | b"UA" | b"DU" | b"AA" | b"UU"
        )
    }
}

pub type Statuses = HashMap<String, FileStatus>;

fn parse_status(bytes: &[u8], prefix: &str) -> Statuses {
    let mut result = Statuses::new();
    let mut records = bytes.split(|&b| b == 0);
    while let Some(record) = records.next() {
        let [index, worktree, b' ', path @ ..] = record else {
            continue;
        };
        let status = FileStatus([*index, *worktree]);
        let original = if status.0.contains(&b'R') || status.0.contains(&b'C') {
            records.next()
        } else {
            None
        };
        let relative = |bytes| std::str::from_utf8(bytes).ok()?.strip_prefix(prefix);
        if let Some(path) = relative(path) {
            result.insert(path.to_string(), status);
        }
        if status.0.contains(&b'R')
            && let Some(path) = original.and_then(relative)
        {
            result.entry(path.to_string()).or_insert(status);
        }
    }
    result
}

fn status(root: &str, prefix: &str) -> Option<Statuses> {
    let out = Command::new("git")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--renames",
            "--",
            ".",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_status(&out.stdout, prefix))
}

pub struct StatusMonitor {
    wake: mpsc::SyncSender<()>,
    stop: Arc<AtomicBool>,
    pub updates: mpsc::Receiver<Statuses>,
}

impl StatusMonitor {
    pub fn start(root: String) -> Self {
        let (wake, events) = mpsc::sync_channel(1);
        let (send, updates) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let quit = Arc::clone(&stop);
        let notify_ui = wake.clone();
        std::thread::spawn(move || {
            let Ok(paths) = run(
                &root,
                &[
                    "rev-parse",
                    "--is-inside-work-tree",
                    "--show-prefix",
                    "--absolute-git-dir",
                    "--path-format=absolute",
                    "--git-common-dir",
                    "--git-path",
                    "index",
                ],
            ) else {
                return;
            };
            let mut paths = paths.lines();
            if paths.next() != Some("true") {
                return;
            }
            let (Some(prefix), Some(meta), Some(common), Some(index)) =
                (paths.next(), paths.next(), paths.next(), paths.next())
            else {
                return;
            };
            let meta = PathBuf::from(meta);
            let common = PathBuf::from(common);
            let index = PathBuf::from(index);
            let watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if event.is_err()
                        || event
                            .as_ref()
                            .is_ok_and(|e| !matches!(e.kind, EventKind::Access(_)))
                    {
                        let _ = notify_ui.try_send(());
                    }
                });
            let _watcher = watcher.ok().map(|mut watcher| {
                let _ = watcher.watch(&meta, RecursiveMode::NonRecursive);
                if common != meta {
                    let _ = watcher.watch(&common, RecursiveMode::NonRecursive);
                }
                let _ = watcher.watch(&common.join("refs"), RecursiveMode::Recursive);
                if let Some(dir) = index.parent()
                    && dir != meta
                    && dir != common
                {
                    let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
                }
                watcher
            });
            while !quit.load(Ordering::Relaxed) {
                if let Some(statuses) = status(&root, prefix)
                    && send.send(statuses).is_err()
                {
                    break;
                }
                if events.recv().is_err() {
                    break;
                }
                // Git commands can generate several events, including atomic index replacement.
                let deadline = Instant::now() + Duration::from_millis(180);
                while !quit.load(Ordering::Relaxed) && Instant::now() < deadline {
                    if events
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        Self {
            wake,
            stop,
            updates,
        }
    }

    pub fn refresh(&self) {
        let _ = self.wake.try_send(());
    }
}

impl Drop for StatusMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_worktree_and_index_states_relative_to_project() {
        let states = parse_status(
            " M app/file.rs\0MM app/both.rs\0?? app/中文 文档.md\0R  app/new name.rs\0app/old name.rs\0UU app/conflict.rs\0D  elsewhere/file.rs\0".as_bytes(),
            "app/",
        );
        assert_eq!(states["file.rs"], FileStatus(*b" M"));
        assert_eq!(states["both.rs"], FileStatus(*b"MM"));
        assert_eq!(states["中文 文档.md"], FileStatus(*b"??"));
        assert_eq!(states["new name.rs"], FileStatus(*b"R "));
        assert_eq!(states["old name.rs"], FileStatus(*b"R "));
        assert!(states["conflict.rs"].conflicted());
        assert_eq!(states.len(), 6);
    }
}
