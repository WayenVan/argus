// argus extension for omp (oh-my-pi). The argus manager writes this file into
// its state directory at startup and loads it into each omp agent it starts
// with `omp -e <this file>`; it is never installed into omp's own config.
// Where argus-hook lives comes in ARGUS_OMP_HOOK.
//
// omp has no command hooks, so this extension plays their part: it turns the
// few lifecycle events argus cares about into flat events, in the shape the
// Rust driver (omp.rs) interprets, and pipes each into `argus-hook omp`.
// Everything here must stay silent and cheap: a failure only loses activity
// tracking, never breaks the agent.
//
// Events are versioned (`v`): agents keep the extension they started with
// while the manager may be upgraded underneath them.

import { spawn } from "node:child_process";

export default function (pi) {
  const hook = process.env.ARGUS_OMP_HOOK;
  if (!hook || !process.env.ARGUS_AGENT_ID || !process.env.ARGUS_SOCKET) return;

  let busy = false; // Between agent_start and the final agent_end.
  const tools = new Map(); // toolCallId -> name, for tools running in parallel.
  const asking = new Set(); // toolCallIds with an open `ask` dialog.
  const approvals = new Set(); // toolCallIds with an open tool approval.
  let prompt; // The prompt of the run about to start.
  let reply; // The run's final reply so far, sent with its end.
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
            const child = spawn(hook, ["omp"], { stdio: ["pipe", "ignore", "ignore"] });
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

  // The text of the last assistant message that has any, like Claude's
  // `last_assistant_message`.
  const replyOf = (messages) => {
    for (const m of [...(messages ?? [])].reverse()) {
      if (m?.role !== "assistant") continue;
      const text =
        typeof m.content === "string"
          ? m.content
          : (m.content ?? []).filter((p) => p?.type === "text").map((p) => p.text ?? "").join("");
      if (text.trim()) return text;
    }
    return undefined;
  };

  // Parallel tool calls can each wait for approval, one dialog at a time.
  // Requests and replies carry their tool-call IDs; activity falls back to
  // the remaining tool only after all dialogs have closed.
  const report = (ctx) => {
    if (!busy) return;
    if (asking.size || approvals.size) return;
    const tool = [...tools.values()].pop();
    if (tool) send("PreToolUse", ctx, { tool_name: tool });
    else send("PostToolUse", ctx);
  };
  const reset = () => {
    busy = false;
    tools.clear();
    asking.clear();
    approvals.clear();
  };

  // session_start fires once; `/new`, resume and fork switch sessions in place.
  const fresh = (source) => (_event, ctx) => {
    reset();
    send("SessionStart", ctx, { source });
  };
  on("session_start", fresh("startup"));
  on("session_switch", fresh("clear"));
  on("session_branch", fresh("clear"));

  on("before_agent_start", (event) => {
    prompt = event.prompt;
  });
  on("agent_start", (_event, ctx) => {
    busy = true;
    reply = undefined;
    send("UserPromptSubmit", ctx, { prompt });
    prompt = undefined;
  });
  on("tool_execution_start", (event, ctx) => {
    tools.set(event.toolCallId, event.toolName);
    // `ask` puts a question to the user and waits for the answer.
    if (event.toolName === "ask") {
      asking.add(event.toolCallId);
      const question = Array.isArray(event.args?.questions) ? event.args.questions[0] : undefined;
      send("PermissionRequest", ctx, {
        request_id: `${event.toolCallId}:ask`,
        interaction_kind: "question",
        tool_name: event.toolName,
        question_text: question?.question,
        choices: Array.isArray(question?.options) ? question.options.map((option) => option?.label) : [],
      });
    }
    report(ctx);
  });
  on("tool_execution_end", (event, ctx) => {
    if (asking.delete(event.toolCallId)) {
      send("PermissionReplied", ctx, { request_id: `${event.toolCallId}:ask` });
    }
    if (approvals.delete(event.toolCallId)) {
      send("PermissionReplied", ctx, { request_id: `${event.toolCallId}:approval` });
    }
    tools.delete(event.toolCallId);
    report(ctx);
  });
  on("tool_approval_requested", (event, ctx) => {
    if (!approvals.has(event.toolCallId)) {
      approvals.add(event.toolCallId);
      send("PermissionRequest", ctx, {
        request_id: `${event.toolCallId}:approval`,
        interaction_kind: "permission",
        tool_name: event.toolName ?? tools.get(event.toolCallId),
      });
    }
    report(ctx);
  });
  on("tool_approval_resolved", (event, ctx) => {
    if (approvals.delete(event.toolCallId)) {
      send("PermissionReplied", ctx, { request_id: `${event.toolCallId}:approval` });
    }
    report(ctx);
  });
  // omp announces its own retries, compaction and other continuations with
  // `willContinue`; only the last agent_end ends the run.
  on("agent_end", (event, ctx) => {
    reply = replyOf(event.messages) ?? reply;
    if (event.willContinue || !busy) return;
    reset();
    const last = event.messages?.findLast((m) => m.role === "assistant");
    if (last?.stopReason === "aborted") send("Interrupt", ctx);
    else if (last?.stopReason === "error") send("StopFailure", ctx);
    else send("Stop", ctx, { last_assistant_message: reply });
  });
}
