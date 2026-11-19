export default function traceTracking(pi) {
  const pending = new Map();
  const session = (ctx) => ({
    session_id: ctx.sessionManager.getSessionId(),
    cwd: ctx.cwd,
    session_title: ctx.sessionManager.getSessionName?.(),
    model: ctx.model?.id,
  });

  for (const [name, phase] of [
    ["session_start", "SessionStart"],
    ["agent_start", "UserPromptSubmit"],
    ["agent_end", "Stop"],
    ["session_shutdown", "SessionEnd"],
  ]) {
    pi.on(name, async (_event, ctx) => {
      if (name === "session_shutdown") pending.clear();
      await send("pi", { ...session(ctx), hook_event_name: phase });
    });
  }

  pi.on("tool_call", async (event, ctx) => {
    if (!["write", "edit"].includes(event.toolName) || typeof event.input?.path !== "string") return;
    const call = {
      ...session(ctx),
      tool_name: event.toolName,
      tool_use_id: event.toolCallId,
    };
    pending.set(event.toolCallId, { ...call, tool_input: { path: event.input.path } });
    await send("pi", { ...call, hook_event_name: "PreToolUse", tool_input: event.input });
  });

  pi.on("tool_execution_end", async (event) => {
    const call = pending.get(event.toolCallId);
    if (!call) return;
    pending.delete(event.toolCallId);
    await send("pi", {
      ...call,
      hook_event_name: event.isError ? "PostToolUseFailure" : "PostToolUse",
    });
  });
}
