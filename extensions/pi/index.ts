import { spawn } from "node:child_process";

/**
 * Pi extension that integrates with clamor for session tracking and state.
 *
 * - session_start: reports session ID to clamor via `clamor hook` so
 *   reload/resume targets the exact session.
 * - before_agent_start: sets state to Working while the model is processing.
 * - turn_end: sets state to Input when the model finishes and awaits input.
 *
 * Requires CLAMOR_AGENT_ID in the environment (set automatically by clamor
 * when spawning agents). Silently no-ops if clamor isn't available.
 *
 * Install: add this extension path to ~/.pi/agent/settings.json:
 *   { "extensions": ["/path/to/clamor/extensions/pi"] }
 * Or symlink into ~/.pi/agent/extensions/clamor-session/
 */

function setState(state: "working" | "input"): void {
  const agentId = process.env.CLAMOR_AGENT_ID;
  if (!agentId) return;
  try {
    spawn("clamor", ["set-state", state, "--agent", agentId], {
      stdio: "ignore",
      env: process.env,
    });
  } catch {
    // clamor not installed or not in PATH — silently ignore
  }
}

export default function (pi: any) {
  pi.on("session_start", async (_event: any, ctx: any) => {
    if (!process.env.CLAMOR_AGENT_ID) return;

    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;

    const payload = JSON.stringify({
      hook_event_name: "SessionStart",
      session_id: sessionId,
    });

    try {
      const child = spawn("clamor", ["hook"], {
        stdio: ["pipe", "ignore", "ignore"],
        env: process.env,
      });
      child.stdin.write(payload);
      child.stdin.end();
      // Don't wait — fire and forget so we don't block pi
    } catch {
      // clamor not installed or not in PATH — silently ignore
    }
  });

  pi.on("before_agent_start", async () => {
    setState("working");
  });

  pi.on("turn_end", async () => {
    setState("input");
  });
}
