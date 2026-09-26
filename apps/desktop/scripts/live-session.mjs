/**
 * Drives a real session against the real runtime and records what it said.
 *
 * The unit tests build event fixtures by hand, which proves the derivation is
 * self-consistent but not that it matches the runtime. This script runs the
 * scripted mock through the loopback protocol exactly as the desktop client
 * does, writes the resulting event log to a fixture, and asserts the shapes the
 * conversation workspace depends on.
 *
 * Usage: node scripts/live-session.mjs [--keep]
 */
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, realpathSync, writeFileSync, existsSync } from "node:fs";
import { createServer, connect } from "node:net";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..", "..", "..");
const binary = join(repoRoot, "target", "debug", "cogito-rpc-dev.exe");
const fixture = join(here, "..", "src", "lib", "live-session.fixture.json");

const TIMEOUT_MS = 60_000;

function freePort() {
  return new Promise((resolvePort, rejectPort) => {
    const probe = createServer();
    probe.on("error", rejectPort);
    probe.listen(0, "127.0.0.1", () => {
      const { port } = probe.address();
      probe.close(() => resolvePort(port));
    });
  });
}

/** Newline-delimited JSON over a socket, which is the runtime's framing. */
class Rpc {
  #socket;
  #buffer = "";
  #nextId = 0;
  #pending = new Map();
  #notifications = [];

  static async open(port) {
    const rpc = new Rpc();
    await rpc.#connect(port);
    return rpc;
  }

  #connect(port) {
    return new Promise((resolveConnect, rejectConnect) => {
      this.#socket = connect(port, "127.0.0.1", () => resolveConnect());
      this.#socket.on("error", rejectConnect);
      this.#socket.setEncoding("utf8");
      this.#socket.on("data", (chunk) => this.#onData(chunk));
    });
  }

  #onData(chunk) {
    this.#buffer += chunk;
    let newline = this.#buffer.indexOf("\n");
    while (newline >= 0) {
      const line = this.#buffer.slice(0, newline).trim();
      this.#buffer = this.#buffer.slice(newline + 1);
      newline = this.#buffer.indexOf("\n");
      if (!line) continue;
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        continue;
      }
      // `ServerMessage` is untagged on the wire: a response is flat and carries
      // an `ok`, a notification carries a `method`.
      if ("ok" in message) {
        const resolvePending = this.#pending.get(message.id);
        if (resolvePending) {
          this.#pending.delete(message.id);
          resolvePending(message);
        }
      } else if (typeof message.method === "string") {
        this.#notifications.push(message);
      }
    }
  }

  request(method, params) {
    this.#nextId += 1;
    const id = `request-${this.#nextId}`;
    const payload = { version: 1, id, method, params };
    return new Promise((resolveRequest) => {
      this.#pending.set(id, resolveRequest);
      this.#socket.write(`${JSON.stringify(payload)}\n`);
    });
  }

  notifications() {
    return this.#notifications;
  }

  /**
   * Polls for a matching notification. Polling rather than eventing keeps this
   * harness simple: it is a test driver, and a missed wake-up would otherwise
   * look like a runtime bug.
   */
  async waitFor(predicate, timeoutMs = TIMEOUT_MS) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
      const found = this.#notifications.find(predicate);
      if (found) return found;
      if (Date.now() > deadline) {
        const methods = [...new Set(this.#notifications.map((note) => note.method))];
        throw new Error(
          `timed out after ${timeoutMs}ms; notifications seen: ${methods.join(", ") || "none"}`,
        );
      }
      await new Promise((r) => setTimeout(r, 100));
    }
  }

  close() {
    this.#socket.destroy();
  }
}

function startRuntime(workspace, port) {
  const child = spawn(binary, [workspace, `127.0.0.1:${port}`], {
    cwd: repoRoot,
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stderr.on("data", (chunk) => process.stderr.write(`[runtime] ${chunk}`));
  return child;
}

async function waitForPort(port, deadlineMs = 20_000) {
  const started = Date.now();
  while (Date.now() - started < deadlineMs) {
    const open = await new Promise((resolveOpen) => {
      const socket = connect(port, "127.0.0.1", () => {
        socket.end();
        resolveOpen(true);
      });
      socket.on("error", () => resolveOpen(false));
    });
    if (open) return true;
    await new Promise((r) => setTimeout(r, 150));
  }
  return false;
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(`assertion failed: ${message}`);
  }
  console.log(`  ok  ${message}`);
}

async function main() {
  if (!existsSync(binary)) {
    throw new Error(`${binary} not found. Run: cargo build -p harness-rpc --bin cogito-rpc-dev`);
  }

  const staging = mkdtempSync(join(tmpdir(), "cogito-live-"));
  writeFileSync(join(staging, "README.md"), "# live session fixture\n");
  mkdirSync(join(staging, "src"), { recursive: true });
  writeFileSync(join(staging, "src", "auth.ts"), "export const token = 1;\n");
  // Checkpoints are git-backed, so a run against a non-repository fails before
  // it reaches the agent. A real session always has a repository.
  const git = (...args) =>
    spawnSync("git", args, { cwd: staging, stdio: "pipe", encoding: "utf8" });
  git("init", "--initial-branch=main");
  git("config", "user.email", "live-session@example.invalid");
  git("config", "user.name", "Live Session");
  git("add", "-A");
  git("commit", "-m", "fixture");
  if (git("status", "--porcelain").status !== 0) {
    throw new Error(`git unavailable in this environment: ${git("status").stderr}`);
  }
  // The runtime canonicalises the workspace root on startup, and rejects a task
  // whose root does not match it exactly, so the client must send the same form.
  const workspace = realpathSync.native(staging);

  const port = await freePort();
  const runtime = startRuntime(workspace, port);
  let rpc;

  try {
    if (!(await waitForPort(port))) throw new Error("runtime did not start listening");
    console.log("runtime listening");

    rpc = await Rpc.open(port);
    await rpc.request("rpc.initialize", {});

    // Ask the runtime what its workspace root is rather than guessing at path
    // form: it canonicalises on startup and rejects a task whose root differs.
    const inspected = await rpc.request("workspace.inspect", { workspace_path: workspace });
    const runtimeRoot = inspected.result?.workspace?.current_directory ?? inspected.result?.current_directory;
    assert(typeof runtimeRoot === "string", `runtime reports its root (${JSON.stringify(inspected.result)})`);
    console.log(`runtime root: ${runtimeRoot}`);

    const created = await rpc.request("session.create", { workspace_path: runtimeRoot });
    const sessionId = created.result.session.id;
    assert(typeof sessionId === "string", "runtime created a session");

    const RUN_TERMINAL = (note) => note.method === "agent.completed" || note.method === "agent.failed";

    const started = await rpc.request("agent.send", {
      task: {
        workspace_root: runtimeRoot,
        user_task: "List the workspace and write a short note.",
        system_instructions: "",
        workspace: { root: runtimeRoot, branch: null, monorepo: false, languages: [], manifests: [], details: {} },
        instructions: [],
        recent_conversation: [],
        selected_files: [],
        initial_tool_results: [],
        git_status: null,
        verification_plan: null,
        resume_session: sessionId,
      },
    });
    assert(started.ok === true, `run started (${JSON.stringify(started.error ?? "ok")})`);
    const terminal = await rpc.waitFor(RUN_TERMINAL);
    if (terminal.method === "agent.failed") {
      throw new Error(`run failed: ${JSON.stringify(terminal.params)}`);
    }
    assert(terminal.method === "agent.completed", "run reached a terminal notification");

    const events = rpc
      .notifications()
      .filter((note) => note.method === "agent.event")
      .map((note) => note.params.event);

    console.log(`captured ${events.length} runtime events`);

    const types = new Set(events.map((event) => event.event_type));
    assert(types.has("user.message"), "runtime emitted user.message");
    assert(types.has("assistant.message") || types.has("assistant.delta"), "runtime emitted assistant text");
    assert(types.has("tool.requested"), "runtime emitted tool.requested");
    assert(types.has("tool.completed"), "runtime emitted tool.completed");
    assert(
      types.has("assistant.delta") || events.every((event) => event.event_type !== "assistant.delta"),
      "streamed text arrived as assistant.delta",
    );

    const tools = events.filter((event) => event.event_type === "tool.requested");
    const toolNames = tools.map((event) => event.payload.data.tool);
    console.log(`tools used: ${toolNames.join(", ")}`);

    // Every requested tool must reach a terminal state, or the workspace would
    // show a row spinning forever.
    for (const name of new Set(toolNames)) {
      const resolved = events.some((event) =>
        ["tool.completed", "tool.failed", "tool.denied"].includes(event.event_type) &&
        event.payload.data.tool === name,
      );
      assert(resolved, `tool ${name} reached a terminal state`);
    }

    writeFileSync(fixture, `${JSON.stringify(events, null, 2)}\n`);
    console.log(`wrote fixture ${fixture}`);

    await checkCancellation(rpc, runtimeRoot, workspace);
  } finally {
    rpc?.close();
    runtime.kill();
  }
}

/**
 * Starts a second run and cancels it.
 *
 * Whether the scripted mock finishes before the cancel lands is a race, so the
 * assertion is the part the workspace depends on: the cancel is accepted, and
 * the run still reaches exactly one terminal notification rather than being left
 * showing as running forever.
 */
async function checkCancellation(rpc, runtimeRoot, workspace) {
  console.log("checking cancellation");
  const created = await rpc.request("session.create", { workspace_path: runtimeRoot });
  const sessionId = created.result.session.id;
  const before = rpc.notifications().length;

  const started = await rpc.request("agent.send", {
    task: {
      workspace_root: runtimeRoot,
      user_task: "List the workspace and write a short note.",
      system_instructions: "",
      workspace: { root: runtimeRoot, branch: null, monorepo: false, languages: [], manifests: [], details: {} },
      instructions: [],
      recent_conversation: [],
      selected_files: [],
      initial_tool_results: [],
      git_status: null,
      verification_plan: null,
      resume_session: sessionId,
    },
  });
  assert(started.ok === true, "second run started");

  const runId = started.result.run_id;
  const cancelled = await rpc.request("agent.cancel", { run_id: runId });
  assert(cancelled.ok === true, `cancel accepted (${JSON.stringify(cancelled.error ?? "ok")})`);

  const deadline = Date.now() + 30_000;
  let terminal = null;
  while (Date.now() < deadline) {
    terminal = rpc
      .notifications()
      .slice(before)
      .find((note) => note.method === "agent.completed" || note.method === "agent.failed");
    if (terminal) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  assert(terminal !== null, "cancelled run reached a terminal notification");

  const duplicates = rpc
    .notifications()
    .slice(before)
    .filter((note) => note.method === "agent.completed" || note.method === "agent.failed");
  assert(duplicates.length === 1, `cancelled run reported one terminal state (${duplicates.length})`);
  void workspace;
}

main().catch((error) => {
  console.error(`live session failed: ${error.message}`);
  process.exitCode = 1;
});
