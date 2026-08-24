#!/usr/bin/env node
/**
 * The rule is that nothing goes unexplained. This is what makes that true.
 *
 * Fails when an interactive control is not wrapped in a Hint, and when the
 * dictionary carries an entry nobody uses. A rule nobody checks is a rule that
 * quietly stops being true the first busy week.
 */
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const SRC = "src";
const files = [];
(function walk(dir) {
  for (const e of readdirSync(dir)) {
    const p = join(dir, e);
    if (statSync(p).isDirectory()) walk(p);
    else if (p.endsWith(".svelte")) files.push(p);
  }
})(SRC);

const dict = readFileSync("src/lib/hints.ts", "utf8");
const defined = new Set([...dict.matchAll(/^\s{2}"([a-z0-9._]+)":\s*\{/gm)].map((m) => m[1]));
const used = new Set();
const problems = [];

for (const f of files) {
  if (f.endsWith("Hint.svelte")) continue;
  const src = readFileSync(f, "utf8");
  for (const m of src.matchAll(/<Hint\s+id="([^"]+)"/g)) used.add(m[1]);
  // A Hint whose id is chosen at runtime, e.g. id={cond ? "a.b" : "c.d"}.
  // Only dotted strings count: an expression also contains the values it is
  // comparing against, and those are not hint ids.
  for (const m of src.matchAll(/<Hint\s+id=\{[^}]*\}/g))
    for (const q of m[0].matchAll(/"([a-z0-9_]+\.[a-z0-9._]+)"/g)) used.add(q[1]);

  // Interactive elements outside a Hint, and not explicitly exempt.
  const lines = src.split("\n");
  lines.forEach((line, i) => {
    if (!/<(button|input|select|textarea|a\s)/.test(line)) return;
    if (line.includes("data-hint-exempt")) return;
    const before = lines.slice(Math.max(0, i - 4), i + 1).join("\n");
    if (before.includes("<Hint")) return;

    // A form field with its own <label for=...> is already explained. The
    // label is the explanation; a tooltip repeating it would be noise.
    const idMatch = line.match(/\sid="([^"]+)"/);
    if (idMatch && src.includes(`<label for="${idMatch[1]}"`)) return;
    problems.push(`${f}:${i + 1}  control with no explanation: ${line.trim().slice(0, 70)}`);
  });
}

for (const id of used) if (!defined.has(id)) problems.push(`unknown hint id used: ${id}`);
for (const id of defined) if (!used.has(id)) problems.push(`hint defined but never shown: ${id}`);

// Exemptions stay visible rather than accumulating unnoticed.
const exempt = files.flatMap((f) =>
  readFileSync(f, "utf8").split("\n")
    .map((l, i) => (l.includes("data-hint-exempt") ? `${f}:${i + 1}  ${l.trim().slice(0, 70)}` : null))
    .filter(Boolean),
);
if (exempt.length) {
  console.log("Hint exemptions (review these):");
  for (const e of exempt) console.log("  " + e);
}

if (problems.length) {
  console.error(`\nHint audit failed with ${problems.length} problem(s):`);
  for (const p of problems) console.error("  " + p);
  process.exit(1);
}
console.log(`Hint audit passed: ${defined.size} explanations, all shown, every control covered.`);
