#!/usr/bin/env node
// todo — ported from the reference's official example extension todo.ts
// Demonstrates that the reference extension *logic*
// carries over: the tool and command in the USER SECTION are a direct
// transliteration of the reference's host.registerTool / host.registerCommand code.
//
// What could not cross the process boundary (the reference runs extensions
// in-process; llm spawns them):
//   - renderCall / renderResult TUI components → the host's own `$ todo`
//     action line and result summary render the call instead
//   - ctx.ui.custom overlay for /todos → the command prints plain text
//   - ctx.sessionManager state reconstruction (the reference stores todo state in
//     session entries so branching rewinds it) → a JSON file at
//     ~/.llm/todo.json; state survives restarts, branches share it
//
// Copy to ~/.llm/extensions/todo, chmod +x, /reload.

// ---------------------------------------------------------------- protocol --
const TOOLS = {};    // name -> {description, parameters, execute}
const COMMANDS = {}; // name -> handler(argText) -> string
const EVENTS = {};   // event name -> [handler(params) -> reply-or-undefined]

function reply(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

// ---------------------------------------------------------------- the reference shim ---
// The subset of the extension API that can be honored out-of-process:
// tools, slash commands, and event hooks. UI / editor / hotkey APIs are
// not available and calling them raises with a clear message.
const the reference = {
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
      `this the reference extension uses "${api}", which is not available to ` +
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
  } else if (kind === "shutdown") {
    // let in-flight async handlers land their replies first
    setTimeout(() => process.exit(0), 100);
  } else {
    ext.unsupported(kind);
  }
}

// ---------------------------------------------------------- USER SECTION ----
// the reference's todo.ts logic, transliterated: same tool name, same actions, same
// /todos command — on the shim instead of the reference's in-process host.
const os = require("os");
const fs = require("fs");
const path = require("path");

const stateFile = path.join(
  process.env.LLM_USER_PATH || path.join(os.homedir(), ".llm"),
  "todo.json",
);

let todos = [];
let nextId = 1;

function load() {
  try {
    const s = JSON.parse(fs.readFileSync(stateFile, "utf8"));
    todos = Array.isArray(s.todos) ? s.todos : [];
    nextId = Number.isInteger(s.nextId) ? s.nextId : 1;
  } catch {
    /* fresh state */
  }
}

function save() {
  fs.mkdirSync(path.dirname(stateFile), { recursive: true });
  fs.writeFileSync(stateFile, JSON.stringify({ todos, nextId }, null, 2) + "\n");
}

load();

ext.registerTool({
  name: "todo",
  description: "Manage a todo list. Actions: list, add (text), toggle (id), clear",
  parameters: {
    type: "object",
    properties: {
      action: {
        type: "string",
        enum: ["list", "add", "toggle", "clear"],
        description: "What to do with the list",
      },
      text: { type: "string", description: "Todo text (for add)" },
      id: { type: "integer", description: "Todo ID (for toggle)" },
    },
    required: ["action"],
  },

  // the reference's execute is (toolCallId, params, signal, onUpdate, ctx); the shim
  // passes just the params — same logic, thinner signature
  execute: async (params) => {
    switch (params.action) {
      case "list":
        return todos.length
          ? todos.map((t) => `[${t.done ? "x" : " "}] #${t.id}: ${t.text}`).join("\n")
          : "No todos";

      case "add": {
        if (!params.text) return "Error: text required for add";
        const todo = { id: nextId++, text: params.text, done: false };
        todos.push(todo);
        save();
        return `Added todo #${todo.id}: ${todo.text}`;
      }

      case "toggle": {
        if (params.id === undefined) return "Error: id required for toggle";
        const todo = todos.find((t) => t.id === params.id);
        if (!todo) return `Todo #${params.id} not found`;
        todo.done = !todo.done;
        save();
        return `Todo #${todo.id} ${todo.done ? "completed" : "uncompleted"}`;
      }

      case "clear": {
        const count = todos.length;
        todos = [];
        nextId = 1;
        save();
        return `Cleared ${count} todos`;
      }

      default:
        return `Unknown action: ${params.action}`;
    }
  },
});

// the reference's /todos opens a full-screen component; out-of-process it prints the
// same list as text
ext.registerCommand("todos", {
  description: "Show all todos",
  handler: async () => {
    if (todos.length === 0) return "No todos yet. Ask the agent to add some!";
    const done = todos.filter((t) => t.done).length;
    const lines = [`${done}/${todos.length} completed`, ""];
    for (const t of todos) {
      lines.push(`${t.done ? "✓" : "○"} #${t.id} ${t.text}`);
    }
    return lines.join("\n");
  },
});

ext.run();
