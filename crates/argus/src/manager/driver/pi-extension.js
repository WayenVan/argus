// argus extension for pi. The argus manager writes this file into its state
// directory at startup and loads it into each pi agent it starts with
// `pi -e <this file>`; it is never installed into pi's own config. Where
// argus-hook lives comes in ARGUS_PI_HOOK.
//
// pi has no command hooks, so this extension plays their part: it turns the
// few lifecycle events argus cares about into flat events, in the shape the
// Rust driver (pi.rs) interprets, and pipes each into `argus-hook pi`.
// Everything here must stay silent and cheap: a failure only loses activity
// tracking, never breaks the agent.
//
// Events are versioned (`v`): agents keep the extension they started with
// while the manager may be upgraded underneath them.

import { spawn } from "node:child_process";

export default function (pi) {
  const hook = process.env.ARGUS_PI_HOOK;
  if (!hook || !process.env.ARGUS_AGENT_ID || !process.env.ARGUS_SOCKET) return;

  // pi re-runs this factory when it replaces the session (`/new`, resume),
  // so this state never outlives one session.
  let busy = false; // Between agent_start and agent_settled.
  let ending; // What the coming agent_settled means, decided at agent_end.
  const tools = new Map(); // toolCallId -> name, for tools running in parallel.
  let queue = Promise.resolve();

  // One argus-hook at a time, so events reach the manager in order.
  const send = (name, ctx, extra = {}) => {
    let session;
    try {
      session = ctx.sessionManager.getSessionId();
    } catch {}
    const event = JSON.stringify({ v: 1, hook_event_name: name, session_id: session, ...extra });
    queue = queue.then(
      () =>
        new Promise((resolve) => {
          try {
            const child = spawn(hook, ["pi"], { stdio: ["pipe", "ignore", "ignore"] });
            child.on("error", resolve);
            child.on("close", resolve);
            child.stdin.on("error", () => {});
            child.stdin.end(event);
          } catch {
            resolve();
          }
        }),
    );
  };

  const on = (name, fn) =>
    pi.on(name, (event, ctx) => {
      try {
        fn(event, ctx);
      } catch {}
    });

  on("session_start", (event, ctx) => {
    // Anything but a fresh start is a new session inside the same agent.
    const source = event.reason === "startup" || event.reason === "reload" ? event.reason : "clear";
    send("SessionStart", ctx, { source });
  });
  on("agent_start", (_event, ctx) => {
    busy = true;
    ending = undefined;
    send("UserPromptSubmit", ctx);
  });
  on("tool_execution_start", (event, ctx) => {
    tools.set(event.toolCallId, event.toolName);
    send("PreToolUse", ctx, { tool_name: event.toolName });
  });
  on("tool_execution_end", (event, ctx) => {
    tools.delete(event.toolCallId);
    if (!busy) return;
    const [still] = tools.values();
    if (still) send("PreToolUse", ctx, { tool_name: still });
    else send("PostToolUse", ctx);
  });
  // A dialog from some extension that waits on the user. Only one opened
  // mid-run blocks the agent; one opened at the prompt (a command) does not.
  on("ui_prompt_start", (_event, ctx) => {
    if (busy) send("PermissionRequest", ctx);
  });
  on("ui_prompt_end", (_event, ctx) => {
    if (busy) send("PermissionReplied", ctx, { tool_name: [...tools.values()].pop() });
  });
  on("agent_end", (event, ctx) => {
    // An interrupted run ends with an aborted signal, whether pi recorded it
    // as `aborted` (mid-stream) or as an `error` (mid-tool).
    const last = event.messages?.at(-1);
    if (ctx.signal?.aborted || last?.stopReason === "aborted") ending = "Interrupt";
    else if (last?.stopReason === "error") ending = "StopFailure";
    else ending = "Stop";
  });
  // Final: retries, compaction and queued follow-ups all happen before it.
  on("agent_settled", (_event, ctx) => {
    if (!busy) return;
    busy = false;
    tools.clear();
    send(ending ?? "Stop", ctx);
  });
}
