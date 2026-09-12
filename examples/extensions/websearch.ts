#!/usr/bin/env node
// websearch.ts — the TypeScript twin of the `websearch` example: same two
// tools (web_search, web_fetch), same /web command, same backend chain —
// in node with no dependencies beyond the standard library.
//
// Node executes this file directly through native type stripping on
// node >= 23.6. On 22.6–23.5 change the shebang to
//   #!/usr/bin/env -S node --experimental-strip-types
// or run it with bun or deno (`#!/usr/bin/env bun` / `deno`), which strip
// types natively too. Stripping erases types but cannot transform enum and
// namespace declarations — this file deliberately uses none.
//
// Install EITHER this or the python `websearch`, not both: the host dedups
// extension entries by file stem, so the two twins would collide.
//
// Search backends (first that answers wins): Brave Search API when
// BRAVE_API_KEY is set (free tier at https://brave.com/search/api);
// keyless fallback chain DuckDuckGo HTML → DDG Instant Answers → Wikipedia.
// Note the built-in `webfetch` tool still exists; `web_fetch` here is the
// plugin-form demonstration. Protocol: newline-delimited JSON on stdio,
// one reply per request id (docs/extensions.md has the reference).

import * as readline from "node:readline";

const HEADERS: Record<string, string> = {
  "User-Agent": "Mozilla/5.0 (compatible; llm-websearch/1.0)",
  Accept: "text/html,application/xhtml+xml,*/*;q=0.8",
};
const FETCH_CAP = 6000; // chars of page text returned to the model

interface ToolSpec {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
}
interface HostMsg {
  id?: number;
  type?: string;
  name?: string;
  args?: unknown; // call_tool: object; run_command: string
  params?: { tool?: string; args?: Record<string, unknown> };
}

const reply = (obj: unknown): void => {
  process.stdout.write(JSON.stringify(obj) + "\n");
};
const msg = (e: unknown): string => (e instanceof Error ? e.message : String(e));

async function httpGet(url: string, timeoutMs = 15000): Promise<{ ctype: string; body: string }> {
  const r = await fetch(url, { headers: HEADERS, signal: AbortSignal.timeout(timeoutMs) });
  // refuse binary bodies (pdf/image/zip/...): decoded as utf-8 they would
  // feed the model mojibake — the built-in webfetch errors by content type
  // for the same reason
  const ctype = (r.headers.get("content-type") ?? "").split(";")[0].trim().toLowerCase();
  const major = ctype.split("/")[0];
  if (
    major === "image" ||
    major === "audio" ||
    major === "video" ||
    ["application/pdf", "application/octet-stream", "application/zip"].includes(ctype)
  ) {
    throw new Error(`binary content (${ctype || "unknown type"}) — fetch it with bash curl instead`);
  }
  // unlike the python twin (capped at 256 KiB) this reads the whole body;
  // the 15s abort bounds a runaway either way
  return { ctype, body: await r.text() };
}

// ------------------------------------------------------------------ fetch --
const BLOCKS = new Set(["p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "pre", "table"]);
const SKIP = new Set(["script", "style", "noscript", "template"]);

// A tag tokenizer over the same block/skip vocabulary the python twin's
// HTMLParser subclass implements: visible text only, newlines at block
// boundaries. A regex tokenizer, like the built-in Rust webfetch's
// html_to_text — no full HTML parser in the dependency set.
function htmlToText(html: string): string {
  const parts: string[] = [];
  let skip = 0;
  let last = 0;
  const tag = /<\/?([a-zA-Z][a-zA-Z0-9]*)\b[^>]*>/g;
  for (let m = tag.exec(html); m; m = tag.exec(html)) {
    if (skip === 0) parts.push(html.slice(last, m.index));
    const name = m[1].toLowerCase();
    if (SKIP.has(name)) skip = Math.max(0, skip + (m[0][1] === "/" ? -1 : 1));
    else if (BLOCKS.has(name)) parts.push("\n");
    last = m.index + m[0].length;
  }
  if (skip === 0) parts.push(html.slice(last));
  return parts.join("").replace(/[ \t]+/g, " ").replace(/\n\s*\n+/g, "\n").trim();
}

async function webFetch(url: string): Promise<string> {
  if (!url.startsWith("http://") && !url.startsWith("https://")) {
    return "error: only http(s) URLs are supported";
  }
  const { ctype, body } = await httpGet(url);
  const isHtml = ctype === "text/html" || ctype === "application/xhtml+xml";
  let text = isHtml ? htmlToText(body) : body.trim(); // JSON/XML/plain pass through
  if (!text) return `${url}\n\n(no text content)`;
  if (text.length > FETCH_CAP) {
    text = text.slice(0, FETCH_CAP) + `\n… truncated at ${FETCH_CAP} chars`;
  }
  return `${url}\n\n${text}`;
}

// ----------------------------------------------------------------- search --
const stripTags = (s: string): string => s.replace(/<[^>]+>/g, "");
const unescapeEntities = (s: string): string =>
  s
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#0?39;|&#x0?27;/gi, "'");

async function brave(query: string, limit: number): Promise<[string, string][]> {
  const key = process.env.BRAVE_API_KEY ?? "";
  if (!key) throw new Error("BRAVE_API_KEY not set");
  const url =
    "https://api.search.brave.com/res/v1/web/search?q=" +
    encodeURIComponent(query) +
    "&count=" +
    Math.min(limit, 20);
  const r = await fetch(url, {
    headers: { "X-Subscription-Token": key, Accept: "application/json", "Accept-Encoding": "identity" },
    signal: AbortSignal.timeout(15000),
  });
  if (r.status === 401 || r.status === 403) {
    throw new Error(`BRAVE_API_KEY rejected (HTTP ${r.status})`);
  }
  const d = (await r.json()) as {
    web?: { results?: { title?: string; description?: string; url: string }[] };
  };
  const out: [string, string][] = [];
  for (const hit of d.web?.results ?? []) {
    const desc = unescapeEntities(stripTags(hit.description ?? ""));
    out.push([(hit.title + " — " + desc).replace(/^[ —]+|[ —]+$/g, ""), hit.url]);
    if (out.length >= limit) break;
  }
  return out;
}

// DuckDuckGo wraps result links as //duckduckgo.com/l/?uddg=<encoded>.
function ddgUnwrap(href: string): string {
  if (href.includes("uddg=")) {
    href = decodeURIComponent(href.split("uddg=")[1].split("&")[0]);
  }
  return href.startsWith("//") ? "https:" + href : href;
}

async function ddgHtml(query: string, limit: number): Promise<[string, string][]> {
  const page = (await httpGet("https://html.duckduckgo.com/html/?q=" + encodeURIComponent(query))).body;
  const out: [string, string][] = [];
  const re = /class="result__a"[^>]*href="([^"]+)"[^>]*>([\s\S]*?)<\/a>/g;
  for (let m = re.exec(page); m && out.length < limit; m = re.exec(page)) {
    out.push([unescapeEntities(stripTags(m[2])).trim(), ddgUnwrap(m[1])]);
  }
  return out;
}

interface DdgTopic {
  FirstURL?: string;
  Text?: string;
  Topics?: DdgTopic[];
}

async function ddgInstant(query: string, limit: number): Promise<[string, string][]> {
  const body = (
    await httpGet("https://api.duckduckgo.com/?q=" + encodeURIComponent(query) + "&format=json&no_html=1&skip_disambig=1")
  ).body;
  const d = JSON.parse(body) as { RelatedTopics?: DdgTopic[] };
  const out: [string, string][] = [];
  const walk = (topics: DdgTopic[]): void => {
    for (const t of topics) {
      if (t.FirstURL && out.length < limit) out.push([t.Text ?? "", t.FirstURL]);
      if (t.Topics) walk(t.Topics);
    }
  };
  walk(d.RelatedTopics ?? []);
  return out;
}

async function wikipedia(query: string, limit: number): Promise<[string, string][]> {
  const body = (
    await httpGet(
      "https://en.wikipedia.org/w/api.php?action=query&list=search&srlimit=" +
        limit +
        "&srsearch=" +
        encodeURIComponent(query) +
        "&format=json",
    )
  ).body;
  const d = JSON.parse(body) as { query?: { search?: { title: string }[] } };
  return (d.query?.search ?? [])
    .slice(0, limit)
    .map((h) => [h.title, "https://en.wikipedia.org/wiki/" + h.title.replace(/ /g, "_")] as [string, string]);
}

const BACKENDS: [string, (query: string, limit: number) => Promise<[string, string][]>][] = [
  ["brave", brave],
  ["duckduckgo", ddgHtml],
  ["duckduckgo instant answers", ddgInstant],
  ["wikipedia (encyclopedia only)", wikipedia],
];

async function webSearch(query: string, limit: number): Promise<string> {
  if (!query.trim()) return "error: empty query";
  const errors: string[] = [];
  for (const [label, backend] of BACKENDS) {
    let results: [string, string][];
    try {
      results = await backend(query, limit);
    } catch (e) {
      errors.push(`${label}: ${msg(e)}`);
      continue;
    }
    if (results.length) {
      return ["via " + label, ...results.map(([t, u], i) => `${i + 1}. ${t}\n   ${u}`)].join("\n");
    }
  }
  const hint = process.env.BRAVE_API_KEY
    ? ""
    : "\n(set BRAVE_API_KEY for full Brave web search; free tier at https://brave.com/search/api)";
  const note = errors.length ? ` (${errors.join("; ")})` : "";
  return `no results for ${JSON.stringify(query)}${note}${hint}`;
}

// ------------------------------------------------------------------- main --
const TOOLS: ToolSpec[] = [
  {
    name: "web_search",
    description:
      "Search the web and return the top result titles and URLs (Brave Search API when " +
      "BRAVE_API_KEY is set, keyless DuckDuckGo/Wikipedia fallback otherwise)",
    parameters: {
      type: "object",
      properties: {
        query: { type: "string", description: "the search query" },
        limit: { type: "integer", description: "max results (default 5)" },
      },
      required: ["query"],
    },
  },
  {
    name: "web_fetch",
    description:
      "Fetch a web page and return its readable text (capped, scripts and styles stripped)",
    parameters: {
      type: "object",
      properties: { url: { type: "string", description: "http(s) URL" } },
      required: ["url"],
    },
  },
];

async function main(): Promise<void> {
  const rl = readline.createInterface({ input: process.stdin });
  for await (const line of rl) {
    const t = line.trim();
    if (!t) continue;
    let m: HostMsg;
    try {
      m = JSON.parse(t) as HostMsg;
    } catch {
      continue;
    }
    const { id, type, name } = m;
    if (type === "initialize") {
      reply({ id, result: { tools: TOOLS, commands: ["web"], events: [] } });
    } else if (type === "call_tool") {
      const args = (m.args ?? {}) as Record<string, unknown>;
      try {
        const out =
          name === "web_search"
            ? await webSearch(String(args.query ?? ""), Number(args.limit) || 5)
            : name === "web_fetch"
              ? await webFetch(String(args.url ?? ""))
              : `unknown tool ${name}`;
        reply({ id, result: out });
      } catch (e) {
        reply({ id, error: `${e instanceof Error ? e.name : "Error"}: ${msg(e)}` });
      }
    } else if (type === "run_command" && name === "web") {
      try {
        reply({ id, result: await webSearch(String(m.args ?? "").trim(), 5) });
      } catch (e) {
        reply({ id, result: `search failed: ${msg(e)}` });
      }
    } else if (type === "shutdown") {
      return;
    }
  }
}

main();
