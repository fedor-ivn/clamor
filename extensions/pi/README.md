# Clamor Session Extension for Pi

Reports pi session IDs to clamor so that reload/resume uses the correct session.

Without this extension, clamor falls back to `pi --continue` which resumes the most recent session in the working directory. With it, clamor captures the exact session ID for precise targeting.

## Install

Symlink into pi's global extensions directory:

```bash
ln -s /path/to/clamor/extensions/pi ~/.pi/agent/extensions/clamor-session
```

Or add to `~/.pi/agent/settings.json`:

```json
{
  "extensions": ["/path/to/clamor/extensions/pi"]
}
```

## How it works

**Session tracking:** On `session_start`, the extension reads `CLAMOR_AGENT_ID` from the environment and pipes the session ID to `clamor hook`. The hook stores it as `resume_token` in clamor state so reload/resume targets the exact session.

**State tracking:** On `before_agent_start` (model is processing), the extension calls `clamor set-state working`. On `turn_end` (model finished, awaiting input), it calls `clamor set-state input`. This keeps the clamor dashboard accurate — agents show Working while the model runs and Input when they need attention.

Silently no-ops when not running under clamor or when clamor isn't installed.
