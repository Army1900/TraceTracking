export const TraceTracking = async ({ directory }) => ({
  event: async ({ event }) => {
    const info = event.properties?.info;
    const sessionID = info?.id ?? event.properties?.sessionID;
    const phase = {
      "session.created": "SessionStart",
      "session.idle": "Stop",
      "session.deleted": "SessionEnd",
    }[event.type];
    if (phase) {
      await send("opencode", {
        session_id: sessionID,
        cwd: info?.directory ?? directory,
        session_title: info?.title,
        hook_event_name: phase,
      });
    }
  },
  "chat.message": async ({ sessionID }) => {
    await send("opencode", { session_id: sessionID, cwd: directory, hook_event_name: "UserPromptSubmit" });
  },
  "tool.execute.before": async ({ sessionID, callID, tool }, { args }) => {
    await fileEvent(directory, sessionID, callID, tool, args, "PreToolUse");
  },
  "tool.execute.after": async ({ sessionID, callID, tool, args }) => {
    await fileEvent(directory, sessionID, callID, tool, args, "PostToolUse");
  },
});
