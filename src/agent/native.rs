use super::{FileReceipt, MISSING, Phase, Receipt, absolute, fingerprint, local_instance};
use crate::store;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{fs, path::Path};

fn string<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value.get(name).and_then(Value::as_str)
}

pub(super) fn translate(tool: &str, payload: Value) -> Result<Option<Receipt>> {
    ensure!(
        matches!(tool, "claude-code" | "codex"),
        "未知的原生 AI 适配器"
    );
    let phase = match string(&payload, "hook_event_name") {
        Some("SessionStart" | "UserPromptSubmit") => Phase::SessionStart,
        Some("Stop" | "Interrupt") => Phase::SessionIdle,
        Some("SessionEnd") => Phase::SessionEnd,
        Some("PreToolUse") => Phase::WriteStart,
        Some("PostToolUse") => Phase::WriteFinish,
        Some("PostToolUseFailure") => Phase::WriteFail,
        _ => return Ok(None),
    };
    let cwd = string(&payload, "cwd").context("Hook 缺少 cwd")?;
    let session_id = string(&payload, "session_id").context("Hook 缺少 session_id")?;
    let mut receipt = Receipt {
        version: 1,
        tool: tool.into(),
        instance: local_instance(),
        session_id: session_id.into(),
        cwd: cwd.into(),
        phase,
        title: string(&payload, "session_title").map(str::to_owned),
        model: string(&payload, "model").map(str::to_owned),
        turn_id: string(&payload, "turn_id")
            .or_else(|| string(&payload, "prompt_id"))
            .map(str::to_owned),
        call_id: string(&payload, "tool_use_id").map(str::to_owned),
        files: Vec::new(),
    };
    if matches!(
        phase,
        Phase::WriteStart | Phase::WriteFinish | Phase::WriteFail
    ) {
        let action = string(&payload, "tool_name").unwrap_or("");
        let input = &payload["tool_input"];
        receipt.files = match (tool, action) {
            ("claude-code", "Write" | "Edit") => {
                let Some(path) = string(input, "file_path") else {
                    return Ok(None);
                };
                let path = absolute(Path::new(cwd), Path::new(path))?;
                let mut file = capture(&path, phase)?;
                if action == "Write"
                    && let Some(content) = string(input, "content")
                {
                    file.expected_after = Some(store::digest(content.as_bytes()));
                } else if action == "Edit"
                    && phase == Phase::WriteStart
                    && let Some((before, after)) = expected_edit(&path, input)
                {
                    file.before = Some(before);
                    file.expected_after = Some(after);
                }
                vec![file]
            }
            ("codex", "apply_patch") => {
                let Some(patch) = string(input, "command") else {
                    return Ok(None);
                };
                patch_files(Path::new(cwd), patch, phase)?
            }
            _ => return Ok(None),
        };
        if receipt.files.is_empty() {
            return Ok(None);
        }
    }
    Ok(Some(receipt))
}

fn capture(path: &Path, phase: Phase) -> Result<FileReceipt> {
    Ok(FileReceipt {
        path: path.to_str().context("Hook 路径需要 UTF-8")?.into(),
        before: (phase == Phase::WriteStart)
            .then(|| fingerprint(path))
            .flatten(),
        expected_after: None,
        observed_after: (phase == Phase::WriteFinish)
            .then(|| fingerprint(path))
            .flatten(),
    })
}

fn small_text(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(store::LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > store::LIMIT {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn expected_edit(path: &Path, input: &Value) -> Option<(String, String)> {
    let body = small_text(path)?;
    let old = string(input, "old_string")?;
    let new = string(input, "new_string")?;
    if old.is_empty() {
        return None;
    }
    let positions = body.match_indices(old).count();
    let expected = if input["replace_all"].as_bool() == Some(true) && positions > 0 {
        body.replace(old, new)
    } else if positions == 1 {
        body.replacen(old, new, 1)
    } else {
        return None;
    };
    Some((
        store::digest(body.as_bytes()),
        store::digest(expected.as_bytes()),
    ))
}

fn patch_files(cwd: &Path, patch: &str, phase: Phase) -> Result<Vec<FileReceipt>> {
    let lines: Vec<_> = patch.lines().collect();
    if lines.first() != Some(&"*** Begin Patch") {
        return Ok(Vec::new());
    }
    let mut i = 1;
    let mut files = Vec::new();
    while i < lines.len() {
        let line = lines[i];
        let (op, relative) = ["Update", "Add", "Delete"]
            .into_iter()
            .find_map(|op| {
                line.strip_prefix(&format!("*** {op} File: "))
                    .map(|path| (op, path))
            })
            .unzip();
        let (Some(op), Some(relative)) = (op, relative) else {
            i += 1;
            continue;
        };
        let source = absolute(cwd, Path::new(relative))?;
        i += 1;
        let destination = lines
            .get(i)
            .and_then(|line| line.strip_prefix("*** Move to: "));
        if destination.is_some() {
            i += 1;
        }
        let start = i;
        while i < lines.len() && !lines[i].starts_with("*** ") {
            i += 1;
        }
        let mut file = capture(&source, phase)?;
        if let Some(destination) = destination {
            // Separate path receipts cover add/delete when the filesystem emits them.
            let dest = absolute(cwd, Path::new(destination))?;
            let mut moved = capture(&dest, phase)?;
            if phase == Phase::WriteStart {
                if let Some((before, after)) = expected_update(&source, &lines[start..i]) {
                    file.before = Some(before);
                    moved.expected_after = Some(after);
                }
                file.expected_after = Some(MISSING.into());
            }
            files.push(moved);
        } else if phase == Phase::WriteStart {
            file.expected_after = match op {
                "Delete" => Some(MISSING.into()),
                "Add" => {
                    let added: Option<Vec<_>> = lines[start..i]
                        .iter()
                        .map(|line| line.strip_prefix('+'))
                        .collect();
                    added.map(|lines| store::digest(format!("{}\n", lines.join("\n")).as_bytes()))
                }
                _ => expected_update(&source, &lines[start..i]).map(|(before, after)| {
                    file.before = Some(before);
                    after
                }),
            };
        }
        files.push(file);
    }
    Ok(files)
}

// Verify simple exact hunks. Ambiguous contexts and fuzzy matching have no expected hash.
fn expected_update(path: &Path, patch: &[&str]) -> Option<(String, String)> {
    let body = small_text(path)?;
    if body.contains('\r') || !body.ends_with('\n') {
        return None;
    }
    let mut file: Vec<&str> = body.lines().collect();
    let mut cursor = 0;
    let mut hunks = patch
        .split(|line| line.starts_with("@@"))
        .filter(|hunk| !hunk.is_empty())
        .peekable();
    hunks.peek()?;
    for hunk in hunks {
        let mut old = Vec::new();
        let mut new = Vec::new();
        for &line in hunk {
            if let Some(text) = line.strip_prefix(' ') {
                old.push(text);
                new.push(text);
            } else if let Some(text) = line.strip_prefix('-') {
                old.push(text);
            } else if let Some(text) = line.strip_prefix('+') {
                new.push(text);
            } else {
                return None;
            }
        }
        if old.is_empty() {
            return None;
        }
        let mut hits = file[cursor..]
            .windows(old.len())
            .enumerate()
            .filter(|(_, lines)| *lines == old.as_slice());
        let index = cursor + hits.next()?.0;
        if hits.next().is_some() {
            return None;
        }
        file.splice(index..index + old.len(), new.iter().copied());
        cursor = index + new.len();
    }
    Some((
        store::digest(body.as_bytes()),
        store::digest(format!("{}\n", file.join("\n")).as_bytes()),
    ))
}
