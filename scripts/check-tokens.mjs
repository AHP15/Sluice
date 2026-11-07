#!/usr/bin/env bun
/**
 * T-092 / D-031 / D-032: fail if any component paints with a raw colour
 * instead of a theme token.
 *
 * The palette, status hues and syntax colours live in src/styles.css as
 * custom properties and reach components only through Tailwind's token
 * classes (`bg-panel`, `text-caution`, ...) or `hsl(var(--x))`. A raw hex,
 * rgb()/hsl() literal, or a Tailwind palette class (`text-red-500`,
 * `bg-black`) would bypass both themes and, worse, D-032's rule that status
 * hues are reserved — so it is a build failure, not a review comment.
 *
 * Usage: bun run check:tokens
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

const ROOT = "src";
const ALLOWED = new Set(["src/styles.css"]); // the one place tokens are defined

const PALETTE =
  "slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose";
const RULES = [
  [/#[0-9a-fA-F]{3}(?:[0-9a-fA-F]{3}(?:[0-9a-fA-F]{2})?)?\b(?=["'`;\s)])/, "raw hex colour"],
  [/\brgba?\s*\(/, "raw rgb() colour"],
  [/\bhsla?\s*\((?!\s*var\()/, "raw hsl() colour (use hsl(var(--token)))"],
  [new RegExp(`\\b(?:bg|text|border|ring|fill|stroke|outline|from|to|via|divide|shadow|decoration|placeholder)-(?:${PALETTE})-\\d{2,3}\\b`), "Tailwind palette class"],
  [/\b(?:bg|text|border|ring|fill|stroke)-(?:black|white)\b/, "Tailwind black/white class"],
];

function* walk(dir) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) yield* walk(p);
    else if (/\.(tsx?|css)$/.test(name)) yield p;
  }
}

let violations = 0;
let files = 0;
for (const file of walk(ROOT)) {
  if (ALLOWED.has(file)) continue;
  files++;
  readFileSync(file, "utf8")
    .split("\n")
    .forEach((line, i) => {
      for (const [rx, why] of RULES) {
        const m = line.match(rx);
        if (m) {
          violations++;
          console.error(`✗ ${file}:${i + 1}: ${why}: ${JSON.stringify(m[0])}`);
        }
      }
    });
}
if (violations > 0) {
  console.error(`\ncheck-tokens: ${violations} raw colour(s); use the theme tokens in src/styles.css.`);
  process.exit(1);
}
console.log(`check-tokens: ${files} file(s) clean — every colour is a theme token.`);
