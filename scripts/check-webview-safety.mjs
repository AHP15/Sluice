#!/usr/bin/env bun
/**
 * T-021 / D-007: fail the build if the webview gains a way to execute database
 * content as markup or code.
 *
 * The threat model is specific. The webview renders arbitrary database
 * content — rows, column names, documents — any of which may contain hostile
 * strings, and D-007's whole payoff is that an XSS there is "someone read one
 * table" rather than credential exfiltration. That bound only holds while
 * values reach the DOM as text nodes.
 *
 * React does the right thing by default, so this checks that nobody opted out.
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, extname } from "node:path";

const ROOTS = ["src"];

const FORBIDDEN = [
  [/dangerouslySetInnerHTML/, "React's innerHTML escape hatch — database values must be text nodes"],
  [/\.innerHTML\s*=/, "direct innerHTML assignment"],
  [/\.outerHTML\s*=/, "direct outerHTML assignment"],
  [/insertAdjacentHTML/, "insertAdjacentHTML parses markup"],
  [/\beval\s*\(/, "eval()"],
  [/new\s+Function\s*\(/, "new Function() is eval by another name"],
  [/document\.write/, "document.write parses markup"],
];

function* walk(dir) {
  for (const entry of readdirSync(dir)) {
    const p = join(dir, entry);
    if (statSync(p).isDirectory()) yield* walk(p);
    else if ([".ts", ".tsx", ".js", ".jsx"].includes(extname(p))) yield p;
  }
}

let violations = 0;
let scanned = 0;

for (const root of ROOTS) {
  for (const file of walk(root)) {
    scanned++;
    const src = readFileSync(file, "utf8");
    src.split("\n").forEach((line, i) => {
      // Allow an explicit, reviewed opt-out marker so this stays a guard rather
      // than an obstacle — but make it loud and greppable.
      if (line.includes("sluice-allow-unsafe-dom")) return;
      for (const [pattern, why] of FORBIDDEN) {
        if (pattern.test(line)) {
          violations++;
          console.error(`✗ ${file}:${i + 1}: ${why}\n    ${line.trim()}`);
        }
      }
    });
  }
}

if (scanned === 0) {
  console.error("check-webview-safety: scanned no files; refusing to pass vacuously.");
  process.exit(2);
}

if (violations > 0) {
  console.error(`\ncheck-webview-safety: ${violations} violation(s).`);
  process.exit(1);
}

console.log(`check-webview-safety: ${scanned} file(s) clean — no markup or code execution paths.`);
