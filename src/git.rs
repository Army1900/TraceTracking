use anyhow::{Context, Result, bail};
use std::process::Command;
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
