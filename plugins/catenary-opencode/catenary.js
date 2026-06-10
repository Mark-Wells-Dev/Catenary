// Catenary OpenCode plugin.
//
// OpenCode has no command-type hook — its only extension surface is an
// in-process JS/TS plugin. So Catenary ships this shim instead of a
// `hooks.json`. It forwards *every* tool call to `catenary hook pre-tool
// --format=opencode` (parity with Catenary's `*`/all-tools matcher on the
// other hosts; the Rust side classifies and no-ops irrelevant tools) and
// enforces the decision it returns.
//
// The plugin is glue: no editing-state logic, no command parsing. All policy
// lives in `catenary hook pre-tool` and is shared verbatim with every other
// host.
//
// Failure policy: fail closed. On any failure (spawn error / `catenary` not on
// PATH, daemon unreachable, timeout, non-zero exit, malformed response) the
// plugin throws, blocking the tool — matching the daemon model's "hooks fail
// closed on daemon crash". The wait is bounded so a wedged daemon blocks rather
// than hangs; the bound must clear daemon cold-start on the session's first
// call (the hook does start-or-connect-and-retry).

// Bounded wait — must clear daemon cold-start on the session's first call.
const TIMEOUT_MS = 5000

// An unreachable daemon would otherwise toast on every blocked tool. Rate-limit
// the "Catenary unavailable" notice to once per plugin (session) lifetime.
let unreachableToastShown = false

async function toastOnce(client, message) {
  if (unreachableToastShown) return
  unreachableToastShown = true
  try {
    await client.tui.showToast({ body: { message, variant: "error" } })
  } catch {
    // Best-effort: a failed toast must not mask the block.
  }
}

export const CatenaryPlugin = async ({ directory, worktree, client }) => ({
  "tool.execute.before": async (input, output) => {
    const payload = JSON.stringify({
      tool: input.tool,
      sessionID: input.sessionID,
      callID: input.callID,
      args: output.args,
      directory,
      worktree,
    })

    let decision
    try {
      // Shell out to the Rust adapter — same binary, same `hook pre-tool`
      // subcommand every other host uses. Bounded wait: a timeout resolves to
      // a block, not a hang.
      const proc = Bun.spawn(["catenary", "hook", "pre-tool", "--format=opencode"], {
        stdin: new TextEncoder().encode(payload),
        stdout: "pipe",
      })
      const text = await Promise.race([
        new Response(proc.stdout).text(),
        new Promise((_, reject) =>
          setTimeout(() => reject(new Error("timeout")), TIMEOUT_MS),
        ),
      ])
      if ((await proc.exited) !== 0) {
        throw new Error("catenary hook pre-tool exited non-zero")
      }
      decision = text.trim() ? JSON.parse(text) : {}
    } catch {
      // Fail closed: Catenary unreachable → block the tool (rate-limited toast).
      await toastOnce(client, "Catenary unavailable — tool blocked")
      throw new Error("Blocked: Catenary enforcement unavailable")
    }

    if (decision.systemMessage) {
      await client.tui.showToast({
        body: { message: decision.systemMessage, variant: "info" },
      })
    }
    if (decision.decision === "deny") {
      throw new Error(decision.reason ?? "Blocked by Catenary")
    }
  },
})
