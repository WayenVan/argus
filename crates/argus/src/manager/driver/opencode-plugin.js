// argus plugin for opencode. The argus manager writes this file into its
// state directory at startup and loads it into each opencode agent it starts
// through OPENCODE_CONFIG_CONTENT; it is never installed into opencode's own
// config. Options: { hook: "<path to argus-hook>" }.
//
// opencode has no command hooks, so this plugin plays their part: it turns
// the few bus events argus cares about into flat events, in the shape the
// Rust driver (opencode.rs) interprets, and pipes each into `argus-hook
// opencode`. Everything here must stay silent and cheap: a failure only
// loses activity tracking, never breaks the agent.
//
// Events are versioned (`v`): agents keep the plugin they started with while
// the manager may be upgraded underneath them.

import { spawn } from "node:child_process";

export const ArgusPlugin = async (_input, options) => {
  const hook = options?.hook;
  if (!hook || !process.env.ARGUS_AGENT_ID || !process.env.ARGUS_SOCKET) return {};

  const parents = new Map(); // subagent session -> the session that started it
  const roots = new Set(); // top-level sessions seen by this process
  const lastTool = new Map(); // root session -> tool it is running
  const failure = new Map(); // root session -> what its coming idle means
  // Root sessions in a turn. An interrupted turn still reports its tool
  // finishing (and pending asks being refused) after it went idle; those
  // must not bring it back to work.
  const busy = new Set();
  // Root session -> its latest assistant message with text: { id, text },
  // sent with the turn's end like Claude's `last_assistant_message`.
  const replies = new Map();
  let queue = Promise.resolve();

  // One argus-hook at a time, so events reach the manager in order.
  const send = (name, session, extra = {}) => {
    const event = JSON.stringify({ v: 1, hook_event_name: name, session_id: session, ...extra });
    queue = queue.then(
      () =>
        new Promise((resolve) => {
          try {
            const child = spawn(hook, ["opencode"], { stdio: ["pipe", "ignore", "ignore"] });
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

  const rootOf = (id) => {
    while (parents.has(id)) id = parents.get(id);
    return id;
  };

  // The top-level session `id` belongs to. A top-level session this process
  // did not create (resumed, or `--continue`) is announced on first sight.
  const root = (id) => {
    const r = rootOf(id);
    if (!roots.has(r)) {
      roots.add(r);
      send("SessionStart", r, { source: "resume" });
    }
    return r;
  };

  const isChild = (id) => parents.has(id);

  const onEvent = (event) => {
    const p = event.properties ?? {};
    switch (event.type) {
      case "session.created": {
        const info = p.info ?? {};
        if (!info.id) return;
        if (info.parentID) {
          parents.set(info.id, info.parentID);
          return;
        }
        const source = roots.size === 0 ? "startup" : "clear";
        roots.add(info.id);
        send("SessionStart", info.id, { source });
        return;
      }
      // Subagents ask too, and the answer is typed into this agent's TUI.
      case "permission.asked":
      case "permission.updated":
      case "question.asked":
        if (p.sessionID) send("PermissionRequest", root(p.sessionID));
        return;
      case "permission.replied":
      case "question.replied":
      case "question.rejected": {
        if (!p.sessionID) return;
        const r = root(p.sessionID);
        if (busy.has(r)) send("PermissionReplied", r, { tool_name: lastTool.get(r) });
        return;
      }
      case "session.error":
        // A subagent's failure reaches its parent as a failed tool call.
        if (!p.sessionID || isChild(p.sessionID)) return;
        failure.set(root(p.sessionID), p.error?.name === "MessageAbortedError" ? "Interrupt" : "StopFailure");
        return;
      case "session.status":
        if (p.sessionID && !isChild(p.sessionID) && p.status?.type === "busy") busy.add(root(p.sessionID));
        return;
      case "session.idle": {
        if (!p.sessionID || isChild(p.sessionID)) return;
        const r = root(p.sessionID);
        // An interrupted turn goes idle twice; only the first ends it.
        if (!busy.delete(r)) return;
        send(failure.get(r) ?? "Stop", r, { last_assistant_message: replies.get(r)?.text });
        failure.delete(r);
        replies.delete(r);
        lastTool.delete(r);
        return;
      }
    }
  };

  const guard =
    (fn) =>
    async (...args) => {
      try {
        fn(...args);
      } catch {}
    };

  return {
    event: guard(({ event }) => onEvent(event)),
    "chat.message": guard((input, output) => {
      if (isChild(input.sessionID)) return;
      const r = root(input.sessionID);
      busy.add(r);
      replies.delete(r);
      const prompt = (output?.parts ?? [])
        .filter((p) => p?.type === "text" && !p.synthetic && !p.ignored)
        .map((p) => p.text ?? "")
        .join("\n");
      send("UserPromptSubmit", r, { prompt: prompt || undefined });
    }),
    // Each finished text part of an assistant message; read, never changed.
    "experimental.text.complete": guard((input, output) => {
      if (isChild(input.sessionID) || !output?.text?.trim()) return;
      const r = root(input.sessionID);
      const last = replies.get(r);
      if (last?.id === input.messageID) last.text += "\n\n" + output.text;
      else replies.set(r, { id: input.messageID, text: output.text });
    }),
    "tool.execute.before": guard((input) => {
      if (isChild(input.sessionID)) return;
      const r = root(input.sessionID);
      lastTool.set(r, input.tool);
      send("PreToolUse", r, { tool_name: input.tool });
    }),
    "tool.execute.after": guard((input) => {
      if (isChild(input.sessionID)) return;
      const r = root(input.sessionID);
      lastTool.delete(r);
      if (busy.has(r)) send("PostToolUse", r);
    }),
  };
};
