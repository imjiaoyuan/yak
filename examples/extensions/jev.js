#!/usr/bin/env node
// jev — TypeSafe's System One model as a yak tool (resident extension).
//
// Copy to ~/.yak/extensions/jev, chmod +x, /reload. It mounts one tool:
//
//   jev {state, questions, model?}
//
// Jev is a discriminative model: it does not generate text. It answers
// typed questions about a state — the text to judge — with probabilities
// and confidence, in one round-trip of a few hundred milliseconds. Three
// question shapes ride in one call (they are answered independently):
//
//   choice: pick one option from a fixed list. criteria is an object of
//     option -> one-line description (a null description is allowed).
//     Answer: the picked option, a probability per option, confidence.
//   noul:   the probability that one statement is true of the state.
//     Answer: a number in [0, 1].
//   score:  place the state on an ordered scale. criteria is an array of
//     levels, weakest first. Answer: a number in [0, 1], probabilities
//     per level, confidence.
//
// Ask several questions in one call and combine the answers in code:
// questions within a call are independent by design; a question that
// depends on another answer is a second call, and that is the exception.
//
// Auth: TYPESAFE_API_KEY (or JEV_API_KEY) in the environment, else the
// call fails — there is no keyless fallback to silently degrade to.
// JEV_MODEL overrides the default "jev-latest"; a per-call model argument
// wins over both. Exec-tier: it leaves the machine.
//
// Protocol: newline-delimited JSON on stdio, one reply per request id
// (docs/extensions.md has the reference). Needs node >= 18 (global fetch).

const API_URL = "https://api.typesafe.ai/v1/systemone";
const DEFAULT_MODEL = "jev-latest";
const TIMEOUT_MS = 30_000;

const TOOLS = [
  {
    name: "jev",
    description:
      "Ask a discriminative model (Jev): typed yes/no, choice or score " +
      "questions about a text, answered with probabilities and confidence. " +
      "Fast (sub-second) and cheap — good for routing, gating and " +
      "classification decisions. Not for text generation.",
    parameters: {
      type: "object",
      properties: {
        state: {
          type: "string",
          description:
            "The text to judge, e.g. an email, a diff, a user request.",
        },
        questions: {
          type: "object",
          description:
            "Question name -> definition. " +
            'choice: {"type":"choice","instructions":..,"criteria":' +
            '{option: "description"|null}}. ' +
            'noul: {"type":"noul","instructions":"statement to test"}. ' +
            'score: {"type":"score","instructions":..,"criteria":' +
            '["weakest", .., "strongest"]}. ' +
            "Several questions ride one call; they are independent.",
        },
        model: {
          type: "string",
          description: "Model id (default: jev-latest)",
        },
      },
      required: ["state", "questions"],
    },
    tier: "exec",
  },
];

function reply(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

function fail(id, message) {
  reply({ id, result: `error: ${message}` });
}

function checkQuestions(questions) {
  if (typeof questions !== "object" || questions === null ||
      Array.isArray(questions)) {
    throw new Error("questions must be an object of name -> definition");
  }
  const names = Object.keys(questions);
  if (names.length === 0) throw new Error("questions is empty");
  for (const name of names) {
    const q = questions[name];
    if (typeof q !== "object" || q === null) {
      throw new Error(`questions.${name} must be an object`);
    }
    if (typeof q.instructions !== "string" || !q.instructions.trim()) {
      throw new Error(`questions.${name}.instructions is required`);
    }
    if (q.type === "choice") {
      const keys = Object.keys(q.criteria || {});
      if (keys.length < 2) {
        throw new Error(`questions.${name}: choice needs >= 2 options`);
      }
    } else if (q.type === "noul") {
      // instructions is the statement; nothing else to check
    } else if (q.type === "score") {
      if (!Array.isArray(q.criteria) || q.criteria.length < 2) {
        throw new Error(`questions.${name}: score needs >= 2 levels`);
      }
    } else {
      throw new Error(
        `questions.${name}.type must be choice, noul or score` +
          (q.type ? ` (got ${q.type})` : "")
      );
    }
  }
}

function renderAnswer(name, answer) {
  if (answer.type === "noul") {
    return `${name}: ${answer.noul} (probability the statement holds)`;
  }
  if (answer.type === "score") {
    const levels = Object.entries(answer.legend || {})
      .map(([k, v]) => `${k}=${v}`)
      .join(", ");
    return (
      `${name}: ${answer.score}` +
      (answer.confidence !== undefined
        ? ` (confidence ${answer.confidence})`
        : "") +
      (levels ? ` [${levels}]` : "")
    );
  }
  const probs = Object.entries(answer.probabilities || {})
    .map(([opt, p]) => `${opt} ${p}`)
    .join(", ");
  return (
    `${name}: ${answer.choice}` +
    (answer.confidence !== undefined
      ? ` (confidence ${answer.confidence})`
      : "") +
    (probs ? ` [${probs}]` : "")
  );
}

async function callJev(args) {
  const key = process.env.TYPESAFE_API_KEY || process.env.JEV_API_KEY;
  if (!key) {
    throw new Error(
      "TYPESAFE_API_KEY (or JEV_API_KEY) is not set. Get a key at " +
        "typesafe.ai and export it, then retry."
    );
  }
  if (typeof args.state !== "string" || !args.state.trim()) {
    throw new Error("state must be a non-empty string");
  }
  const questions = args.questions;
  checkQuestions(questions);

  const model = args.model || process.env.JEV_MODEL || DEFAULT_MODEL;
  const body = { model, state: args.state, questions };
  const res = await fetch(API_URL, {
    method: "POST",
    headers: {
      authorization: `Bearer ${key}`,
      "content-type": "application/json",
    },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(TIMEOUT_MS),
  });
  const text = await res.text();
  let parsed;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error(`HTTP ${res.status}: ${text.slice(0, 300)}`);
  }
  if (!res.ok) {
    const detail =
      (parsed.error && (parsed.error.message || parsed.error)) ||
      parsed.message || text.slice(0, 300);
    throw new Error(`HTTP ${res.status}: ${detail}`);
  }
  const answers = parsed.answers;
  if (!answers || typeof answers !== "object") {
    throw new Error(`unexpected response shape: ${text.slice(0, 300)}`);
  }
  const lines = Object.keys(questions)
    .filter((n) => answers[n])
    .map((n) => renderAnswer(n, answers[n]));
  const missing = Object.keys(questions).filter((n) => !answers[n]);
  if (missing.length) lines.push(`missing answers: ${missing.join(", ")}`);
  const usage = parsed.usage
    ? ` [${parsed.usage.input_tokens}in/${parsed.usage.output_tokens}out tokens]`
    : "";
  return `${lines.join("\n")}${usage}`;
}

async function dispatch(line) {
  let req;
  try {
    req = JSON.parse(line);
  } catch {
    return;
  }
  if (req.type === "initialize") {
    reply({
      id: req.id,
      result: {
        tools: TOOLS,
        commands: [],
        events: [],
      },
    });
  } else if (req.type === "call_tool") {
    try {
      reply({ id: req.id, result: await callJev(req.args || {}) });
    } catch (e) {
      fail(req.id, e.message);
    }
  } else if (req.type === "interrupt") {
    // the fetch's own AbortSignal.timeout bounds the call; nothing to stop
  } else if (req.type === "shutdown") {
    process.exit(0);
  }
}

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
