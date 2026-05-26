import { spawn } from "node:child_process";

/**
 * Pi extension that integrates with clamor for session tracking and state.
 *
 * - session_start: sets state to Working via `clamor set-state working`,
 *   passing the session ID as `--session-token` so clamor can persist it as
 *   resume_token for token-based session resume.
 * - before_agent_start: sets state to Working (fallback if session_start
 *   didn't fire).
 * - turn_end: sets state to Input when the model finishes and awaits input.
 *
 * Requires CLAMOR_AGENT_ID in the environment (set automatically by clamor
 * when spawning agents). Silently no-ops if clamor isn't available.
 *
 * Install: add this extension path to ~/.pi/agent/settings.json:
 *   { "extensions": ["/path/to/clamor/extensions/pi"] }
 * Or symlink into ~/.pi/agent/extensions/clamor-session/
 */

function setState(state: "working" | "input", sessionToken?: string): void {
  const agentId = process.env.CLAMOR_AGENT_ID;
  if (!agentId) return;
  try {
    const args = ["set-state", state, "--agent", agentId];
    if (sessionToken) {
      args.push("--session-token", sessionToken);
    }
    spawn("clamor", args, {
      stdio: "ignore",
      env: process.env,
    });
  } catch {
    // clamor not installed or not in PATH — silently ignore
  }
}

export default function (pi: any) {
  pi.on("session_start", async (_event: any, ctx: any) => {
    const sessionId = ctx.sessionManager.getSessionId();
    if (!sessionId) return;
    setState("working", sessionId);
  });

  pi.on("before_agent_start", async () => {
    setState("working");
  });

  pi.on("turn_end", async () => {
    setState("input");
  });
}
