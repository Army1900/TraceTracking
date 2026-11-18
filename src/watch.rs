use crate::store;
use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
pub struct Worker {
    stop: Arc<AtomicBool>,
    pub updates: mpsc::Receiver<String>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
pub fn start(root: String) -> Worker {
    let stop = Arc::new(AtomicBool::new(false));
    let quit = stop.clone();
    let (out, updates) = mpsc::channel();
    thread::spawn(move || {
        let job = || -> Result<()> {
            let mut db = store::database()?;
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(
                    store::data_dir()?.join(format!("{}.lock", store::digest(root.as_bytes()))),
                )?;
            let mut version = -1;
            loop {
                match lock.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) => {
                        let current: i64 = db.query_row("PRAGMA data_version", [], |r| r.get(0))?;
                        if current != version {
                            let _ = out.send("监听中 · 共享记录".into());
                            version = current;
                        }
                        if quit.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        thread::sleep(Duration::from_millis(400));
                    }
                    Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
                }
            }
            let (tx, rx) = mpsc::channel();
            let mut watcher = notify::recommended_watcher(move |ev| {
                let _ = tx.send(ev);
            })?;
            watcher.watch(root.as_ref(), RecursiveMode::Recursive)?;
            let filter = store::rules(&db, &root)?;
            let mut last_update = Instant::now();
            store::scan_with(&mut db, &root, root.as_ref(), &filter, true, || {
                if last_update.elapsed() > Duration::from_millis(400) {
                    let _ = out.send("建立基线".into());
                    last_update = Instant::now();
                }
                !quit.load(Ordering::Relaxed)
            })?;
            let _ = out.send("监听中".into());
            while !quit.load(Ordering::Relaxed) {
                let Ok(first) = rx.recv_timeout(Duration::from_millis(500)) else {
                    continue;
                };
                let mut paths = HashSet::new();
                let mut full = false;
                let mut moves = Vec::new();
                let mut ingest = |ev: notify::Result<notify::Event>| match ev {
                    Ok(e) if e.need_rescan() => full = true,
                    Ok(e) if !matches!(e.kind, notify::EventKind::Access(_)) => {
                        if matches!(
                            e.kind,
                            notify::EventKind::Modify(notify::event::ModifyKind::Name(
                                notify::event::RenameMode::Both
                            ))
                        ) && e.paths.len() == 2
                        {
                            moves.push((e.paths[0].clone(), e.paths[1].clone()));
                        }
                        paths.extend(e.paths)
                    }
                    Err(_) => full = true,
                    _ => {}
                };
                ingest(first);
                let until = Instant::now() + Duration::from_millis(180);
                while let Ok(ev) = rx.recv_timeout(until.saturating_duration_since(Instant::now()))
                {
                    ingest(ev);
                    if Instant::now() >= until {
                        break;
                    }
                }
                if full {
                    paths = HashSet::from([PathBuf::from(&root)]);
                }
                let filter = store::rules(&db, &root)?;
                let before = db.total_changes();
                for (a, b) in moves {
                    store::moved(&mut db, &root, &a, &b, &filter)?;
                }
                for path in paths {
                    if path.starts_with(&root)
                        && let Err(e) = store::scan(&mut db, &root, &path, &filter, false)
                    {
                        let _ = out.send(e.to_string());
                    }
                }
                if db.total_changes() != before {
                    let _ = out.send("监听中".into());
                }
            }
            Ok(())
        };
        if let Err(e) = job() {
            let _ = out.send(format!("监听暂停 · {e}"));
        }
    });
    Worker { stop, updates }
}
