/**
 * Catenary LSP Integration for OpenCode
 *
 * Provides LSP-powered code intelligence:
 * - Watches file edits and runs LSP diagnostics after each write
 * - Exposes `catenary-grep` / `catenary-glob` for semantic symbol search
 * - Tracks editing session and cleans up on session end
 *
 * Platform notes:
 *   - On Unix/Linux/macOS with the daemon running: full LSP diagnostics, grep, glob
 *   - On Windows: daemon is not yet supported; grep/glob/diagnostics fall back to
 *     `catenary doctor` health-check only. The `catenary-doctor` tool always works.
 *   - Set `CATENARY_BIN` env var to override the binary path.
 *   - Set `CATENARY_WSL=true` to run Catenary from WSL (recommended on Windows
 *     for full LSP features — requires the patched binary in WSL at
 *     ~/.cargo/bin/catenary).
 *
 * Setup:
 *   1. Install Catenary:
 *        Unix:     cargo install catenary-mcp
 *        Windows:  cargo install catenary-mcp   (or use WSL for full features)
 *        WSL path: ~/.cargo/bin/catenary (build from catenary-dev with OpenCode patches)
 *   2. Configure language servers: ~/.config/catenary/config.toml
 *   3. Place this file in: ~/.config/opencode/plugin/catenary.mjs
 *   4. Restart OpenCode
 *   5. Verify:  opencode  →  /doctor  or run `catenary-doctor`
 */

import { spawn } from "node:child_process";
import { resolve } from "node:path";
import { homedir } from "node:os";

// ── Catenary binary resolution ─────────────────────────────────────────────────

/** Resolved Catenary binary path. Respects CATENARY_BIN env var. */
function resolveCatenaryBin() {
  if (process.env.CATENARY_BIN) return process.env.CATENARY_BIN;

  const home = homedir();

  // WSL mode: run the patched Linux binary from WSL
  // Set CATENARY_WSL=true in your environment to enable this.
  if (process.env.CATENARY_WSL === "true" || process.env.CATENARY_WSL === "1") {
    return resolve(home, ".cargo", "bin", "catenary");
  }

  if (process.platform === "win32") {
    return resolve(home, ".cargo", "bin", "catenary.exe");
  }
  return resolve(home, ".cargo", "bin", "catenary");
}

const CATENARY_BIN = resolveCatenaryBin();

/** Whether we are running via WSL. */
const IS_WSL = process.env.CATENARY_WSL === "true" || process.env.CATENARY_WSL === "1";

/** Whether the current platform supports Catenary daemon (Unix only). */
const SUPPORTS_DAEMON = process.platform !== "win32" || IS_WSL;

// ── State ─────────────────────────────────────────────────────────────────────
const editedFiles = new Set();
let sessionRegistered = false;
let daemonAvailable = null; // null = unknown, true = up, false = down

// ── Subprocess runner ─────────────────────────────────────────────────────────

/**
 * Run a Catenary CLI command and return stdout.
 * On WSL: prepends "wsl" to run the command in the Linux environment.
 * Silently returns null on any error — plugins must never break OpenCode's flow.
 */
async function runCatenary(args, options = {}) {
  const useWsl = IS_WSL;
  const cmd = useWsl ? "wsl" : CATENARY_BIN;
  const cmdArgs = useWsl ? [CATENARY_BIN, ...args] : args;

  return new Promise((resolve) => {
    const proc = spawn(cmd, cmdArgs, { timeout: 30_000, ...options });
    let stdout = "";
    let stderr = "";

    proc.stdout?.on("data", (d) => (stdout += String(d)));
    proc.stderr?.on("data", (d) => (stderr += String(d)));

    proc.on("close", (code) => {
      if (code === 0) {
        resolve(stdout.trim() || null);
      } else {
        if (stderr) {
          // Trim noisy output — only surface the first line
          const firstLine = stderr.split("\n")[0];
          console.error(`[catenary] ${args[0]} exited ${code}: ${firstLine.slice(0, 160)}`);
        }
        resolve(null);
      }
    });

    proc.on("error", (err) => {
      console.error(`[catenary] failed to spawn${useWsl ? " WSL " + CATENARY_BIN : " " + CATENARY_BIN}: ${err.message}`);
      resolve(null);
    });
  });
}

/**
 * Send a hook request to the Catenary daemon via IPC.
 * Uses `catenary hook <subcommand> --format=claude` (stdin/stdout).
 * Returns null on failure (plugin must not break host flow).
 */
async function catenaryHookIpc(method, extraFields = {}) {
  const cwd = process.cwd();
  const payload = {
    method,
    format: "claude",
    cwd,
    session_id: null,
    ...extraFields,
    host_payload: {
      tool_name: extraFields.tool_name || null,
      cwd,
    },
  };

  const hookSubcommand = methodToHookSubcommand(method);
  if (!hookSubcommand) return null;

  const args = ["hook", hookSubcommand, "--format=claude"];
  const cmd = IS_WSL ? "wsl" : CATENARY_BIN;
  const cmdArgs = IS_WSL ? [CATENARY_BIN, ...args] : args;

  return new Promise((resolve) => {
    const proc = spawn(cmd, cmdArgs, { timeout: 30_000 });
    let stdout = "";
    let stderr = "";

    proc.stdout?.on("data", (d) => (stdout += String(d)));
    proc.stderr?.on("data", (d) => (stderr += String(d)));

    proc.on("close", (code) => {
      if (code === 0 && stdout.trim()) {
        try {
          resolve(JSON.parse(stdout.trim()));
        } catch {
          resolve(null);
        }
      } else {
        if (stderr) {
          const firstLine = stderr.split("\n")[0];
          console.error(`[catenary] hook ${hookSubcommand} error: ${firstLine.slice(0, 160)}`);
        }
        resolve(null);
      }
    });

    proc.on("error", (err) => {
      console.error(`[catenary] hook IPC error: ${err.message}`);
      resolve(null);
    });

    proc.stdin?.write(JSON.stringify(payload) + "\n");
    proc.stdin?.end();
  });
}

function methodToHookSubcommand(method) {
  const map = {
    "session-start/clear-editing": "session-start",
    "session-end/cleanup": "session-end",
    "pre-agent/turn-start": "pre-agent",
    "pre-tool/editing-state": "pre-tool",
    "post-agent/require-release": "post-agent",
    "pre-tool/editing-start": "pre-tool",
    "pre-tool/editing-stop": "pre-tool",
  };
  return map[method] || null;
}

// ── Editing lifecycle ──────────────────────────────────────────────────────────

async function trackEdit(filePath) {
  const absPath = resolve(filePath);
  editedFiles.add(absPath);

  if (!sessionRegistered && SUPPORTS_DAEMON) {
    sessionRegistered = true;
    await catenaryHookIpc("session-start/clear-editing");
  }
}

async function endSession() {
  if (sessionRegistered) {
    await catenaryHookIpc("session-end/cleanup");
    sessionRegistered = false;
  }
  editedFiles.clear();
}

// ── Tool name detection ───────────────────────────────────────────────────────

const FILE_WRITE_TOOLS = new Set([
  "write", "write_file", "edit", "edit_file", "patch_file",
  "replace_in_file", "insert_content_at", "multi_replace_file",
  "Write", "Edit", "PatchFile", "ReplaceInFile",
]);

function isFileWriteTool(toolName) {
  return FILE_WRITE_TOOLS.has(toolName?.toLowerCase());
}

function extractFilePath(toolName, args) {
  if (!args || typeof args !== "object") return null;

  const keys = ["file_path", "filePath", "path", "file", "target", "target_file", "targetPath", "filePath"];
  for (const key of keys) {
    const val = args[key];
    if (typeof val === "string" && val.length > 0 && val.length < 4096) return val;
  }

  if (Array.isArray(args)) {
    for (const item of args) {
      if (typeof item === "object" && item !== null) {
        for (const key of keys) {
          const val = item[key];
          if (typeof val === "string" && val.length > 0) return val;
        }
      }
    }
  }
  return null;
}

// ── Plugin export ─────────────────────────────────────────────────────────────

export default async function CatenaryPlugin(ctx) {
  const platformNote = IS_WSL
    ? "Running via WSL (full LSP features enabled)"
    : SUPPORTS_DAEMON
      ? "Unix platform (full daemon support)"
      : "Windows platform (daemon not yet supported — LSP grep/glob/diagnostics require WSL or Unix)";

  console.error(`[catenary] Catenary LSP plugin loaded | ${platformNote}`);
  console.error(`[catenary] binary: ${CATENARY_BIN}`);

  return {
    hooks: {
      "session.created": async () => {
        console.error("[catenary] session started");
        daemonAvailable = null;
        sessionRegistered = false;
        editedFiles.clear();
      },

      "session.deleted": async () => {
        await endSession();
        console.error("[catenary] session ended");
      },

      "session.compacted": async () => {
        console.error("[catenary] session compacted (LSP session alive, editedFiles preserved)");
      },

      "tool.execute.before": async (ctx, input) => {
        const toolName = ctx?.tool;
        if (!toolName) return;

        if (isFileWriteTool(toolName)) {
          const filePath = extractFilePath(toolName, ctx?.input || input);
          if (filePath) {
            await trackEdit(filePath);
          }
        }
      },

      "tool.execute.after": async (ctx, result) => {
        const toolName = ctx?.tool;
        if (!toolName) return;

        if (result?.error || result?.content?.[0]?.is_error) return;

        if (isFileWriteTool(toolName) && editedFiles.size > 0 && SUPPORTS_DAEMON) {
          if (daemonAvailable === null) {
            const health = await runCatenary(["doctor", "--json"]).catch(() => null);
            daemonAvailable = health !== null;
          }

          if (daemonAvailable) {
            const diagResult = await runCatenary(["hook", "post-tool", "--format=claude"]).catch(() => null);
            if (diagResult && diagResult.systemMessage) {
              console.error(`[catenary] ${diagResult.systemMessage.split("\n").slice(0, 2).join(" | ")}`);
            }
          }
        }
      },

      "message.created": async (ctx) => {
        const text = String(ctx?.message || "").toLowerCase();
        if (text.includes("catenary") || text.includes("lsp") || text.includes("diagnostic")) {
          console.error(
            `[catenary] LSP plugin active. ` +
            `Files tracked: ${editedFiles.size}. ` +
            `Platform: ${platformNote}. ` +
            `Run 'catenary-doctor' to check LSP server health.`
          );
        }
      },
    },

    tool: {
      /**
       * LSP-powered semantic search using language-server symbol context.
       * Requires Catenary daemon on Unix, or WSL mode on Windows.
       * Falls back to plain-text doctor output on Windows without WSL.
       */
      "catenary-grep": {
        description:
          "LSP-powered semantic search. Searches with symbol context from LSP servers " +
          "(type signatures, definitions, cross-file references). " +
          "Requires Catenary daemon (Unix) or CATENARY_WSL=true (Windows with WSL binary).",
        args: {
          pattern: {
            type: "string",
            description: "Regex pattern to search for (Rust/PCRE syntax). Use | for alternation.",
            required: true,
          },
          scope: {
            type: "string",
            description: "File or directory path to search within. Defaults to current working directory.",
            required: false,
          },
        },
        async execute({ pattern, scope }, context) {
          const args = ["grep", pattern];
          if (scope) args.push(scope);

          const result = await runCatenary(args, {
            cwd: context?.directory || process.cwd(),
          });

          if (result === null) {
            if (!SUPPORTS_DAEMON && !IS_WSL) {
              return {
                content: [{
                  type: "text",
                  text:
                    "[catenary-grep] Daemon not available on Windows without WSL.\n" +
                    "Set CATENARY_WSL=true in your environment and ensure the patched\n" +
                    "Catenary binary is at ~/.cargo/bin/catenary in WSL.\n" +
                    "Alternatively, run 'catenary-doctor' to at least check LSP server health.",
                }],
              };
            }
            return {
              content: [{
                type: "text",
                text:
                  "[catenary-grep] No results or Catenary daemon unavailable.\n" +
                  "Run 'catenary-doctor' to check LSP server health.",
              }],
            };
          }

          return { content: [{ type: "text", text: result }] };
        },
      },

      /**
       * Browse files with LSP symbol outlines.
       * Requires Catenary daemon on Unix, or WSL mode on Windows.
       */
      "catenary-glob": {
        description:
          "Browse files with LSP document-symbol outlines (functions, classes, exports per file). " +
          "Much more informative than ls. Requires Catenary daemon (Unix) or CATENARY_WSL=true (Windows).",
        args: {
          paths: {
            type: "string",
            description: "File or directory path(s). Supports glob patterns.",
            required: true,
          },
        },
        async execute({ paths }, context) {
          const result = await runCatenary(["glob", paths], {
            cwd: context?.directory || process.cwd(),
          });

          if (result === null) {
            if (!SUPPORTS_DAEMON && !IS_WSL) {
              return {
                content: [{
                  type: "text",
                  text:
                    "[catenary-glob] Daemon not available on Windows without WSL.\n" +
                    "Set CATENARY_WSL=true to enable full LSP features via WSL.",
                }],
              };
            }
            return {
              content: [{
                type: "text",
                text: "[catenary-glob] No results or daemon unavailable. Run 'catenary-doctor' to diagnose.",
              }],
            };
          }
          return { content: [{ type: "text", text: result }] };
        },
      },

      /**
       * LSP-aware find-and-replace across files.
       * Preview mode shows per-file match counts. Use --in-place to apply.
       */
      "catenary-sed": {
        description:
          "LSP-aware find-and-replace across files. Preview mode shows per-file match counts.\n" +
          "Use --in-place to apply. Requires daemon (Unix) or CATENARY_WSL=true (Windows).",
        args: {
          pattern: { type: "string", description: "Regex pattern to match.", required: true },
          replacement: { type: "string", description: "Replacement text. Use $1, $2... for capture groups.", required: true },
          paths: { type: "string", description: "File or directory path(s).", required: true },
          in_place: { type: "boolean", description: "If true, apply edits. If false (default), show preview only.", required: false },
        },
        async execute({ pattern, replacement, paths, in_place }, context) {
          const args = ["sed", pattern, replacement, paths];
          if (in_place) args.push("--in-place");

          const result = await runCatenary(args, {
            cwd: context?.directory || process.cwd(),
          });

          if (result === null) {
            if (!SUPPORTS_DAEMON && !IS_WSL) {
              return {
                content: [{
                  type: "text",
                  text:
                    "[catenary-sed] Daemon not available on Windows without WSL.\n" +
                    "Set CATENARY_WSL=true to enable full LSP features via WSL.",
                }],
              };
            }
            return {
              content: [{
                type: "text",
                text: "[catenary-sed] Failed. Run 'catenary-doctor' to diagnose.",
              }],
            };
          }
          return { content: [{ type: "text", text: result }] };
        },
      },

      /**
       * Print LSP diagnostics for all files edited in this session.
       * Call after making several edits to see errors and warnings across all touched files.
       */
      "catenary-diagnostics": {
        description:
          "Print LSP errors and warnings for all files edited in this session.\n" +
          "Automatically tracks file-write tools. Requires daemon (Unix) or CATENARY_WSL=true (Windows).",
        args: {},
        async execute({}, context) {
          if (editedFiles.size === 0) {
            return {
              content: [{
                type: "text",
                text: "[catenary-diagnostics] No files tracked yet in this session.\nEdit some files first, then run this tool.",
              }],
            };
          }

          const result = await runCatenary(["diagnostics"]);

          if (result === null) {
            if (!SUPPORTS_DAEMON && !IS_WSL) {
              return {
                content: [{
                  type: "text",
                  text:
                    "[catenary-diagnostics] Daemon not available on Windows without WSL.\n" +
                    `Files tracked: ${Array.from(editedFiles).join(", ")}\n` +
                    "Set CATENARY_WSL=true to enable full LSP features via WSL.",
                }],
              };
            }
            return {
              content: [{
                type: "text",
                text:
                  "[catenary-diagnostics] Catenary daemon unavailable.\n" +
                  `Files tracked: ${Array.from(editedFiles).join(", ")}`,
              }],
            };
          }
          return { content: [{ type: "text", text: result }] };
        },
      },

      /**
       * Health-check for Catenary LSP integration.
       * Always works — verifies LSP servers are installed and responding.
       * This is the most reliable tool on Windows (no daemon required).
       */
      "catenary-doctor": {
        description:
          "Run Catenary health check. Verifies all configured LSP servers are installed\n" +
          "and responding. Always works on all platforms. Use when LSP features seem broken.",
        args: {},
        async execute({}, context) {
          const result = await runCatenary(
            ["doctor", "--json"],
            { cwd: context?.directory || process.cwd() }
          );

          if (result === null) {
            const textResult = await runCatenary(["doctor"]);
            if (textResult === null) {
              return {
                content: [{
                  type: "text",
                  text:
                    `[catenary-doctor] Catenary not found or not working.\n` +
                    `Expected binary at: ${CATENARY_BIN}\n` +
                    `Install: cargo install catenary-mcp\n` +
                    `WSL mode: set CATENARY_WSL=true and ensure ~/.cargo/bin/catenary exists in WSL\n` +
                    `Docs:    https://twowells.github.io/Catenary/`,
                }],
              };
            }
            return { content: [{ type: "text", text: textResult }] };
          }

          try {
            const health = JSON.parse(result);
            const lines = [];
            for (const [lang, info] of Object.entries(health)) {
              if (info && typeof info === "object") {
                const status = info.ok !== false ? "✓" : "✗";
                const name = info.name || lang;
                lines.push(`  ${status} ${lang}: ${name}`);
              }
            }
            return {
              content: [{
                type: "text",
                text:
                  `[catenary-doctor] LSP server health:\n` +
                  (lines.length > 0 ? lines.join("\n") : `  (raw: ${result})`),
              }],
            };
          } catch {
            return { content: [{ type: "text", text: result }] };
          }
        },
      },
    },
  };
}
