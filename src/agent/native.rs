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
        matches!(tool, "claude-code" | "codex" | "opencode" | "pi" | "dsh"),
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
    if session_id.is_empty() {
        return Ok(None);
    }
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
            ("claude-code" | "dsh", "Write" | "Edit")
            | ("opencode" | "pi" | "dsh", "write" | "edit")
            | ("opencode", "multiedit") => {
                let Some(file) = text_write(tool, action, Path::new(cwd), input, phase)? else {
                    return Ok(None);
                };
                vec![file]
            }
            ("codex" | "opencode", "apply_patch") => {
                let key = if tool == "codex" {
                    "command"
                } else {
                    "patchText"
                };
                let Some(patch) = string(input, key) else {
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

fn text_write(
    tool: &str,
    action: &str,
    cwd: &Path,
    input: &Value,
    phase: Phase,
) -> Result<Option<FileReceipt>> {
    let key = match tool {
        "opencode" => "filePath",
        "pi" => "path",
        _ => "file_path",
    };
    let Some(path) = string(input, key).filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let path = if tool == "pi" {
        path.strip_prefix('@').unwrap_or(path)
    } else {
        path
    };
    let path = if let Some(relative) = path.strip_prefix("~/") {
        dirs::home_dir().context("找不到用户目录")?.join(relative)
    } else {
        Path::new(path).to_path_buf()
    };
    let path = absolute(cwd, &path)?;
    let mut file = capture(&path, phase)?;
    if matches!(action, "Write" | "write")
        && let Some(content) = string(input, "content")
    {
        file.expected_after = Some(store::digest(content.as_bytes()));
    } else if phase == Phase::WriteStart
        && let Some((before, after)) = expected_edit(&path, tool, input)
    {
        file.before = Some(before);
        file.expected_after = Some(after);
    }
    Ok(Some(file))
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

fn expected_edit(path: &Path, tool: &str, input: &Value) -> Option<(String, String)> {
    let mut body = small_text(path)?;
    let before = store::digest(body.as_bytes());
    let (old_key, new_key, all_key) = match tool {
        "opencode" => ("oldString", "newString", "replaceAll"),
        "pi" => ("oldText", "newText", "replaceAll"),
        _ => ("old_string", "new_string", "replace_all"),
    };
    let edits = input["edits"]
        .as_array()
        .map_or(std::slice::from_ref(input), Vec::as_slice);
    for edit in edits {
        let old = string(edit, old_key)?;
        let new = string(edit, new_key)?;
        if old.is_empty() {
            return None;
        }
        let mut positions = body.match_indices(old);
        let (index, _) = positions.next()?;
        let all = edit[all_key].as_bool() == Some(true);
        let count = if all {
            1 + positions.count()
        } else if positions.next().is_none() {
            1
        } else {
            return None;
        };
        let remaining = body.len() - old.len() * count;
        if new.len().checked_mul(count)?.checked_add(remaining)? as u64 > store::LIMIT {
            return None;
        }
        if all {
            body = body.replace(old, new);
        } else {
            body.replace_range(index..index + old.len(), new);
        }
    }
    Some((before, store::digest(body.as_bytes())))
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
