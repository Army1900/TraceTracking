function fileEvent(cwd, sessionID, id, tool, input, phase) {
  if (!["write", "edit", "multiedit", "apply_patch"].includes(tool)) return;
  return send("opencode", {
    cwd,
    session_id: sessionID,
    tool_name: tool,
    tool_use_id: id,
    tool_input: phase === "PreToolUse" || tool === "apply_patch" ? input : { filePath: input?.filePath },
    hook_event_name: phase,
  });
}
