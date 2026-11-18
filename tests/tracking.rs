use rusqlite::Connection;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
struct Fixture {
    dir: PathBuf,
    monitor: Option<Child>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(p) = self.monitor.as_mut() {
            let _ = p.kill();
            let _ = p.wait();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}
fn cli(data: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tt"));
    cmd.env("TT_DATA_DIR", data);
    cmd
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(12);
    while !condition() {
        assert!(Instant::now() < deadline, "等待文件监听超时");
        thread::sleep(Duration::from_millis(40));
    }
}
#[test]
fn native_watcher_tracks_plain_project() {
    let dir = std::env::temp_dir().join(format!(
        "trace-native-{}-{}",
        std::process::id(),
        chrono::Local::now().timestamp_micros()
    ));
    let mut fixture = Fixture {
        dir: dir.clone(),
        monitor: None,
    };
    let root = dir.join("普通目录");
    let data = dir.join("data");
    fs::create_dir_all(&root).unwrap();
    let p = root.join("config.yaml");
    fs::write(&p, "timeout: 30\n").unwrap();
    fs::write(root.join("trace.log"), "ignored").unwrap();
    assert!(
        cli(&data)
            .arg("ignore")
            .arg(&root)
            .arg("*.log")
            .status()
            .unwrap()
            .success()
    );
    fixture.monitor = Some(
        cli(&data)
            .arg("watch")
            .arg(&root)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let db = Connection::open(data.join("history.sqlite3")).unwrap();
    let count = |sql: &str| db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
    wait_for(|| count("SELECT count(*) FROM events WHERE kind='B'") == 1);
    fs::write(&p, "timeout: 60\n").unwrap();
    assert!(
        cli(&data)
            .arg("hook")
            .arg(&root)
            .args(["config.yaml", "test-session"])
            .status()
            .unwrap()
            .success()
    );
    wait_for(|| count("SELECT count(*) FROM events WHERE kind='M' AND source='test-session'") == 1);
    assert_eq!(
        count("SELECT count(*) FROM events WHERE path='trace.log'"),
        0
    );
    fs::remove_file(&p).unwrap();
    wait_for(|| count("SELECT count(*) FROM events WHERE kind='D'") == 1);
}
