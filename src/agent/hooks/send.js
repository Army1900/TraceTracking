import { spawn } from "node:child_process";

function send(tool, event) {
  if (!event.session_id || !event.cwd) return;
  return new Promise((done) => {
    try {
      const input = JSON.stringify(event);
      if (Buffer.byteLength(input) > 8 * 1024 * 1024) return done();
      const child = spawn("tt", ["ai-hook", tool], {
        stdio: ["pipe", "ignore", "ignore"],
        windowsHide: true,
      });
      const deadline = setTimeout(() => {
        child.kill();
        done();
      }, 2000);
      function finish() {
        clearTimeout(deadline);
        done();
      }
      child.once("error", finish);
      child.once("close", finish);
      child.stdin.on("error", () => {});
      child.stdin.end(input);
    } catch {
      done();
    }
  });
}
