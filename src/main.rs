mod agent;
mod git;
mod refs;
mod store;
mod syntax;
mod ui;
mod view;
mod watch;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use std::{env, io::IsTerminal, path::Path};
fn root(s: &str) -> Result<String> {
    let p = Path::new(s).canonicalize()?;
    if !p.is_dir() {
        bail!("需要项目目录")
    }
    Ok(p.to_str().context("目录名称需要 UTF-8")?.into())
}
fn main() -> Result<()> {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.first().is_some_and(|s| s == "--help" || s == "-h") {
        println!(
            "TraceTracking · 项目变更浏览器\n\n tt [目录]                 打开项目\n tt add <目录>             注册项目\n tt watch <目录>           持续记录（无界面）\n tt hook <目录> <路径> <来源>   写入成功后记录来源\n tt ai-hook <工具>          从 stdin 接收 AI 工具 Hook\n tt ai-event               从 stdin 接收统一 AI 事件\n tt ai-hook-config <工具>   输出可选的 Hook 配置或插件源码\n tt ignore <目录> <规则>    添加忽略规则\n tt unignore <目录> <规则>  移除忽略规则"
        );
        return Ok(());
    }
    match args.first().map(String::as_str) {
        Some("ai-hook") if args.len() == 2 => return agent::capture(Some(&args[1])),
        Some("ai-event") if args.len() == 1 => return agent::capture(None),
        Some("ai-hook-config") if args.len() == 2 => return agent::hook_config(&args[1]),
        Some("ai-hook" | "ai-event" | "ai-hook-config") => {
            bail!(
                "使用 tt ai-hook <claude-code|codex|opencode|pi|dsh>、tt ai-event 或 tt ai-hook-config <工具>"
            )
        }
        _ => {}
    }
    let mut db = store::database()?;
    match args.first().map(String::as_str) {
        Some("hook") if args.len() == 4 => {
            let r = root(&args[1])?;
            let p = Path::new(&r).join(&args[2]).canonicalize()?;
            let rel = p
                .strip_prefix(&r)?
                .to_str()
                .context("路径需要 UTF-8")?
                .replace(std::path::MAIN_SEPARATOR, "/");
            if p.metadata()?.len() > store::LIMIT {
                bail!("仅支持文本快照范围内的文件")
            }
            let hash = store::digest(&std::fs::read(p)?);
            let now = chrono::Local::now().timestamp();
            let tx = db.unchecked_transaction()?;
            let n=tx.execute("UPDATE events SET source=? WHERE id=(SELECT id FROM files WHERE root=? AND path=?) AND new=? AND stamp>=? AND kind IN ('A','M') AND source=''",params![args[3],r,rel,hash,now-15])?;
            if n == 0 {
                tx.execute(
                    "INSERT INTO hooks VALUES(?,?,?,?,?)",
                    params![r, rel, hash, args[3], now],
                )?;
            }
            tx.commit()?;
            return Ok(());
        }
        Some("ignore" | "unignore") if args.len() == 3 => {
            let r = root(&args[1])?;
            store::update_rule(&mut db, &r, &args[2], args[0] == "ignore")?;
            return Ok(());
        }
        Some("hook" | "ignore" | "unignore") => bail!("参数不完整，使用 tt --help"),
        _ => {}
    }
    if args.first().is_some_and(|s| s == "watch") {
        let root = root(args.get(1).context("需要项目目录")?)?;
        let _agent_inbox = agent::InboxWorker::start();
        let monitor = watch::start(root);
        while let Ok(message) = monitor.updates.recv() {
            if message.starts_with("监听暂停") {
                bail!("{message}")
            }
            println!("{message}");
        }
        return Ok(());
    }
    let add = args.first().is_some_and(|s| s == "add");
    let r = if args.is_empty() {
        store::strings(
            &db,
            "SELECT root FROM projects ORDER BY used DESC LIMIT 1",
            [],
        )?
        .pop()
        .unwrap_or(root(".")?)
    } else {
        root(args.get(usize::from(add)).context("需要目录")?)?
    };
    db.execute(
        "INSERT OR REPLACE INTO projects VALUES(?,?)",
        params![r, chrono::Local::now().timestamp_millis()],
    )?;
    if add {
        println!("{r}");
        return Ok(());
    }
    if !std::io::stdout().is_terminal() {
        bail!("请在交互式终端中运行 tt")
    }
    ui::run(db, r)
}

#[cfg(test)]
mod language_tests;
