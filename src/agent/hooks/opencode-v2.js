export default {
  id: "tracetracking",
  async setup(ctx) {
    const directory = ctx.location.directory;
    await ctx.tool.hook("execute.before", async (event) => {
      await fileEvent(directory, event.sessionID, event.id, event.tool, event.input, "PreToolUse");
    });
    await ctx.tool.hook("execute.after", async (event) => {
      const phase = event.status === "completed" ? "PostToolUse" : "PostToolUseFailure";
      await fileEvent(directory, event.sessionID, event.id, event.tool, event.input, phase);
    });

    const phases = {
      "session.created": "SessionStart",
      "session.execution.started": "UserPromptSubmit",
      "session.execution.succeeded": "Stop",
      "session.execution.failed": "Stop",
      "session.deleted": "SessionEnd",
    };
    const controller = new AbortController();
    void (async () => {
      for await (const event of ctx.event.subscribe({
        types: Object.keys(phases),
        signal: controller.signal,
      })) {
        await send("opencode", {
          cwd: event.data.location?.directory ?? event.location?.directory ?? directory,
          session_id: event.data.sessionID,
          session_title: event.data.title,
          hook_event_name: phases[event.type],
        });
      }
    })().catch(() => {});
    return () => controller.abort();
  },
};
