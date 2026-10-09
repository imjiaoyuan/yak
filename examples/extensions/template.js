#!/usr/bin/env node
// yak extension template (js, host-compatible) — copy to
// ~/.yak/extensions/<name> and edit the USER SECTION at the bottom.
// The shim below speaks the agent's stdio protocol; the user section is
// written in the host API style (registerTool / registerCommand /
// on(event)), which maps onto it directly.

// protocol
const TOOLS = {};    // name -> {description, parameters, execute}
// seconds to ask the host for your own call deadline (it defaults to 120s);
// raise it for a tool that runs a build or another agent
const TOOL_TIMEOUT = null;
const COMMANDS = {}; // name -> handler(argText) -> string
const EVENTS = {};   // event name -> [handler(params) -> reply-or-undefined]

function reply(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

// host shim
// The subset of the host API that can be honored out-of-process:
// tools, slash commands, and event hooks. UI / editor / hotkey APIs are
// not available and calling them raises with a clear message.
const host = {
  registerTool(def) {
    if (!def || !def.name) throw new Error("registerTool: name required");
    TOOLS[def.name] = {
      description: def.description || "",
      parameters: def.parameters || { type: "object", properties: {} },
      execute: def.execute || (() => ""),
    };
  },

  registerCommand(name, def) {
    const handler = typeof def === "function" ? def : def && def.handler;
    if (!handler) throw new Error("registerCommand: handler required");
    COMMANDS[name] = handler;
  },

  on(event, handler) {
    (EVENTS[event] = EVENTS[event] || []).push(handler);
  },

  unsupported(api) {
    throw new Error(
      `this extension uses "${api}", which is not available to ` +
        `out-of-process extensions (tools, commands and event hooks are)`
    );
  },

  run() {
    let buffer = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (chunk) => {
      buffer += chunk;
      let idx;
      while ((idx = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, idx).trim();
        buffer = buffer.slice(idx + 1);
        if (line) dispatch(line);
      }
    });
  },
};

async function dispatch(line) {
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    return;
  }
  const kind = req.type;
  if (kind === "initialize") {
    reply({
      id: req.id,
      result: {
        tools: Object.entries(TOOLS).map(([name, t]) => ({
          name,
          description: t.description,
          parameters: t.parameters,
        })),
        commands: Object.keys(COMMANDS),
        events: Object.keys(EVENTS),
        ...(TOOL_TIMEOUT ? { tool_timeout: TOOL_TIMEOUT } : {}),
      },
    });
  } else if (kind === "call_tool") {
    const tool = TOOLS[req.name];
    try {
      if (!tool) throw new Error(`unknown tool ${req.name}`);
      const out = await tool.execute(req.args || {});
      reply({ id: req.id, result: typeof out === "string" ? out : JSON.stringify(out) });
    } catch (e) {
      reply({ id: req.id, result: `error: ${e.message}` });
    }
  } else if (kind === "run_command") {
    try {
      const handler = COMMANDS[req.name];
      if (!handler) throw new Error(`unknown command ${req.name}`);
      const out = await handler(req.args || "");
      reply({ id: req.id, result: typeof out === "string" ? out : JSON.stringify(out) });
    } catch (e) {
      reply({ id: req.id, result: `error: ${e.message}` });
    }
  } else if (kind === "event") {
    let result;
    for (const handler of EVENTS[req.name] || []) {
      try {
        const r = await handler(req.params || {});
        if (result === undefined && r && typeof r === "object") result = r;
      } catch (e) {
        process.stderr.write(`${req.name} handler failed: ${e.message}\n`);
      }
    }
    reply({ id: req.id, result: result ?? null });
  } else if (kind === "interrupt") {
    // ctrl+c abandoned the call we are running: stop child processes here.
    // This loop only hears it once the call returns — a tool that owns
    // children reads stdin on a second thread instead, see subagent.py
  } else if (kind === "shutdown") {
    // let in-flight async handlers land their replies first
    setTimeout(() => process.exit(0), 100);
  } else {
    host.unsupported(kind);
  }
}

// USER SECTION
// host-style extension code goes here. Examples:

host.registerTool({
  name: "now",
  description: "Current local time",
  parameters: { type: "object", properties: {} },
  execute: async () => new Date().toString(),
});

host.on("tool_call", (params) => {
  // return { decision: "deny", reason: "no" } to block a call, or
  // { args: {...} } to rewrite its arguments
  return undefined;
});

host.run();
