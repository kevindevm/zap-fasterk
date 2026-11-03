import { readFileSync, writeFileSync } from "node:fs";

const args = process.argv.slice(2);
const getArg = (name, fallback = undefined) => {
  const index = args.indexOf(`--${name}`);
  if (index === -1 || index + 1 >= args.length) {
    return fallback;
  }
  return args[index + 1];
};

const inputPath = getArg("input");
const outputPath = getArg("output", inputPath);
const model = getArg("model", process.env.OLLAMA_MODEL || "qwen3:14b");
const host = (getArg("host", process.env.OLLAMA_HOST || "http://127.0.0.1:11434") || "").replace(
  /\/$/,
  "",
);

if (!inputPath) {
  process.stderr.write(
    "Usage: node scripts/polish-release-notes.mjs --input <notes.md> [--output <notes.md>] [--model <name>] [--host <url>]\n",
  );
  process.exit(2);
}

const source = readFileSync(inputPath, "utf8");
if (!source.trim()) {
  process.stderr.write(`Empty input: ${inputPath}\n`);
  process.exit(2);
}

// Commit links ([abc1234](url)) become plain [abc1234] tags before the LLM
// sees the text: short tags survive rewrites, long URLs don't. Re-attached
// deterministically afterwards from this map.
const linkByTag = new Map();
const taggedSource = source.replace(/\[([0-9a-f]{7})\]\(https?:\/\/[^)\s]+\)/g, (match, tag) => {
  const url = match.slice(tag.length + 3, -1);
  linkByTag.set(tag, url);
  return `[${tag}]`;
});

const system = [
  "You rewrite software release notes for end users.",
  "Input is markdown with ### sections and bullet lines, ending with a Full changelog link line.",
  "Each bullet ends with a reference tag like [abc1234]: copy it verbatim to the end of your rewritten bullet. Never invent, move, or drop tags.",
  "Rewrite EACH bullet independently, in order, one output bullet per input bullet: same count, same order. Never merge or split bullets.",
  "Rules: plain natural English; short bullets; user-facing wording (what changed for the user, not commit-speak).",
  "NEVER invent, remove, or merge features, numbers, or versions: keep every factual item.",
  "Keep the section headers. Keep the final Full changelog link line exactly as-is.",
  "No em dashes anywhere.",
  "Output only the markdown, no commentary.",
].join(" ");

let response;
try {
  response = await fetch(`${host}/api/chat`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      model,
      stream: false,
      options: { temperature: 0.2 },
      messages: [
        { role: "system", content: system },
        { role: "user", content: taggedSource },
      ],
    }),
  });
} catch (error) {
  process.stderr.write(`Ollama is not reachable at ${host}: ${error.message}\n`);
  process.stderr.write("Start Ollama first (ollama serve) and pull a model (ollama pull qwen3:14b).\n");
  process.exit(1);
}

if (!response.ok) {
  process.stderr.write(`Ollama returned HTTP ${response.status}. Is the model '${model}' pulled?\n`);
  process.exit(1);
}

const body = await response.json().catch(() => null);
const polished = body?.message?.content?.trim() ?? "";

if (!polished) {
  process.stderr.write("Ollama returned an empty rewrite; leaving the input untouched.\n");
  process.exit(1);
}

const countBullets = (text) =>
  text.split("\n").filter((line) => line.trim().startsWith("- ")).length;
const inputBullets = countBullets(taggedSource);
const outputBullets = countBullets(polished);
if (inputBullets !== outputBullets) {
  process.stderr.write(
    `Refusing to write: bullet count changed ${inputBullets} -> ${outputBullets}. The model merged or dropped items.\n`,
  );
  process.exit(1);
}

const tagPattern = /\[([0-9a-f]{7})\]/g;
const inputTags = new Set([...taggedSource.matchAll(tagPattern)].map((m) => m[1]));
const outputTags = new Set([...polished.matchAll(tagPattern)].map((m) => m[1]));
const missingTags = [...inputTags].filter((tag) => !outputTags.has(tag));
if (missingTags.length > 0) {
  process.stderr.write("Refusing to write: reference tags lost in the rewrite.\n");
  process.exit(1);
}
const extraTags = [...outputTags].filter((tag) => !linkByTag.has(tag));
if (extraTags.length > 0) {
  process.stderr.write("Refusing to write: invented reference tags in the rewrite.\n");
  process.exit(1);
}

const linked = polished.replace(tagPattern, (match, tag) => {
  const url = linkByTag.get(tag);
  return url ? `[${tag}](${url})` : match;
});

writeFileSync(outputPath, `${linked}\n`);
console.log(`Polished with ${model}: ${outputPath}`);

// Unload the model to free VRAM/RAM. Best effort: Ollama also unloads
// idle models on its own after its keep_alive window.
try {
  await fetch(`${host}/api/generate`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ model, keep_alive: 0 }),
  });
  console.log(`Unloaded model: ${model}`);
} catch (error) {
  process.stderr.write(`Warning: could not unload model ${model}: ${error.message}\n`);
}
