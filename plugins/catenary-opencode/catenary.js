// Catenary OpenCode plugin.
//
// OpenCode has no command-type hook — its only extension surface is an
// in-process JS/TS plugin. So Catenary ships this plugin instead of a
// `hooks.json`. It has two responsibilities:
//
//   1. Enforcement (`tool.execute.before`): forward *every* tool call to
//      `catenary hook pre-tool --format=opencode` (parity with Catenary's
//      `*`/all-tools matcher on the other hosts; the Rust side classifies and
//      no-ops irrelevant tools) and enforce the decision it returns.
//
//   2. Config (`config`): two by-reference mutations of OpenCode's cached
//      config object, in order. First — unconditionally, before anything
//      fallible — inject the MCP heartbeat (`config.mcp.catenary`), the
//      long-lived local client that keeps the daemon + warm LSP pool alive for
//      the session (the daemon exits on last client disconnect). Then
//      regenerate the per-worktree runtime instructions file from the binary —
//      the same SSOT payload `catenary primer` prints, resolved live so it
//      carries the session's allow surface — and append its path to
//      `config.instructions`. Teaching is runtime-only: regeneration needs only
//      the `catenary` binary, the user config, and a writable tmp dir (nothing
//      daemon-side — command allowance resolves from local config), so a regen
//      failure means the install itself is broken, and generic teaching
//      pointing at dead commands would be worse than silence. It therefore
//      fails open to *no* teaching: a regen failure leaves `config.instructions`
//      unmodified, with only the already-injected heartbeat surviving.
//      OpenCode's `Instruction.system()` re-reads every instruction file from
//      disk on each prompt step, so the live surface rides every request and
//      survives compaction with zero per-request plugin work. This mirrors
//      Claude Code's SessionStart: the instructions file is ambient (read like a
//      CLAUDE.md), not an injected turn.
//
// Integration is plugin-only: `catenary install opencode` writes just this one
// Catenary-owned file (the plugin) and never edits the user-owned
// `opencode.json` — the heartbeat that used to be merged into it now rides the
// `config` hook above.
//
// The plugin is glue: no editing-state logic, no command parsing, no teaching
// content. All policy and all payload text live in the `catenary` binary and are
// shared verbatim with every other host.
//
// ── Risk ledger (source-verified against opencode v1.17.13, commit 04d236c) ──
// The `config` hook receives the cached config object and mutation-by-reference
// is the registration channel: appending to `config.instructions` is a working
// de-facto contract, not a first-class API.
//   * v2-API watch: `packages/plugin/src/v2/` is being built out; migrate to its
//     first-class instructions/context registration contract when one lands.
//   * Fallback channel: if this by-reference append stops taking effect, the
//     `experimental.chat.system.transform` hook can inject per-request system
//     blocks instead (per-request work — used only if the ambient file breaks).
//   * Runtime-only teaching: there is no shipped static fallback. Regeneration
//     needs only the `catenary` binary, the user config, and a writable tmp dir
//     (nothing daemon-side — command allowance resolves from local config), so a
//     regen failure means the install itself is broken; generic teaching
//     pointing at dead commands would then be worse than silence, so the hook
//     fails open to no teaching. Disabling the plugin drops everything together
//     — teaching, the heartbeat, and enforcement all ride this one plugin.

import { tmpdir } from "node:os"
import { join, dirname } from "node:path"
import { mkdir, writeFile } from "node:fs/promises"
import { createHash } from "node:crypto"

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

export const CatenaryPlugin = async ({ directory, worktree, client }) => {
  // Plugin-owned location for the runtime-regenerated instructions file: a
  // stable per-worktree path under the OS temp dir, so concurrent projects on
  // one host never clobber each other's live surface and the user's repo is
  // never polluted. Absolute, so OpenCode resolves it regardless of config dir.
  const scope = createHash("sha256")
    .update(worktree || directory || "catenary")
    .digest("hex")
    .slice(0, 16)
  const instructionsPath = join(tmpdir(), "catenary-opencode", `instructions-${scope}.md`)

  // Regenerate the instructions file from the binary. Runs the same SSOT emitter
  // the other hosts use (`catenary hook session-start`), in the project's
  // directory so the per-root `.catenary.toml` build tool resolves. Returns the
  // path on success; throws on any failure so the caller can fall back.
  async function regenerateInstructions() {
    const proc = Bun.spawn(["catenary", "hook", "session-start", "--format=opencode"], {
      cwd: worktree || directory || undefined,
      stdin: "ignore",
      stdout: "pipe",
      stderr: "ignore",
    })
    const text = await Promise.race([
      new Response(proc.stdout).text(),
      new Promise((_, reject) =>
        setTimeout(() => reject(new Error("timeout")), TIMEOUT_MS),
      ),
    ])
    if ((await proc.exited) !== 0) {
      throw new Error("catenary hook session-start exited non-zero")
    }
    if (!text.trim()) {
      throw new Error("catenary hook session-start produced no payload")
    }
    await mkdir(dirname(instructionsPath), { recursive: true })
    await writeFile(instructionsPath, text)
    return instructionsPath
  }

  return {
    // Config: inject the MCP heartbeat, then regenerate + register the live
    // instructions file. Both are by-reference mutations of the cached config.
    // Fires on config load (session/process start) and any reload;
    // `Instruction.system()` re-reads instruction files every prompt step, so
    // registering once keeps the surface live for the session.
    config: async (config) => {
      // MCP heartbeat — inject unconditionally and *first*, two assignments
      // before anything fallible, so a later regen failure can never skip it.
      // `??=` defers to any pre-existing entry (e.g. an older merged install),
      // which is harmless by construction.
      config.mcp ??= {}
      config.mcp.catenary ??= { type: "local", command: ["catenary"], enabled: true }

      // Live teaching — regenerate the runtime instructions file and register it.
      // Teaching is runtime-only: there is no static fallback. Fail-open — a cold
      // window / unreachable binary means the install itself is broken (regen
      // needs only the binary and local config, nothing daemon-side), and generic
      // teaching pointing at dead commands would be worse than silence. So a regen
      // failure returns early, leaving `config.instructions` unmodified and only
      // the already-injected heartbeat surviving. Never break OpenCode's config
      // load.
      let path
      try {
        path = await regenerateInstructions()
      } catch {
        return
      }
      if (!Array.isArray(config.instructions)) config.instructions = []
      if (!config.instructions.includes(path)) config.instructions.push(path)
    },

    // Enforcement: forward every tool call to the shared Rust adapter.
    //
    // Failure policy: fail closed. On any failure (spawn error / `catenary` not
    // on PATH, daemon unreachable, timeout, non-zero exit, malformed response)
    // the plugin throws, blocking the tool — matching the daemon model's "hooks
    // fail closed on daemon crash". The wait is bounded so a wedged daemon blocks
    // rather than hangs; the bound clears daemon cold-start on the session's
    // first call (the hook does start-or-connect-and-retry).
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
  }
}
