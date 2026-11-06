#!/usr/bin/env bun
/**
 * T-019 / R-005: fail the build if the exact-numeric edit path gains a way to
 * turn a value into a JavaScript number.
 *
 * D-006 ships `int8` and `numeric` as tagged strings so they survive display.
 * R-005 is that they are then corrupted on *write* if anything between the grid
 * cell and the bound parameter calls `parseFloat` or uses `<input type=number>`
 * — silently, two layers away from the protection, which is why it passes
 * review. So it is checked mechanically on every file on that path.
 *
 * Comments are skipped: these files explain what they must not do.
 */
import { existsSync, readFileSync } from "node:fs";

/** Every file between an exact cell and the value that is bound. */
const EDIT_PATH = [
  "src/lib/exactNumeric.ts",
  "src/components/ExactNumericEditor.tsx",
  "src/components/ResultGrid.tsx",
];

const FORBIDDEN = [
  [/\bparseFloat\s*\(/, "parseFloat rounds exact numerics"],
  [/\bparseInt\s*\(/, "parseInt loses precision past 2^53"],
  [/\bNumber\s*\(/, "Number() rounds exact numerics"],
  [/\bNumber\.parse(Float|Int)\b/, "Number.parseFloat/parseInt round exact numerics"],
  [/\bBigInt\s*\(/, "the rule is that digits are never converted — compare them as text"],
  [/valueAsNumber/, "valueAsNumber is an f64"],
  [/type\s*=\s*\{?\s*["']number["']/, "<input type=number> coerces through an f64"],
  [/\bMath\.\w+\s*\(/, "Math.* operates on f64"],
];

function codeLines(src) {
  let inBlock = false;
  return src.split("\n").map((line) => {
    let out = line;
    if (inBlock) {
      const end = out.indexOf("*/");
      if (end === -1) return "";
      out = out.slice(end + 2);
      inBlock = false;
    }
    out = out.replace(/\/\*.*?\*\//g, "");
    const start = out.indexOf("/*");
    if (start !== -1) {
      out = out.slice(0, start);
      inBlock = true;
    }
    // Line comments, but not `//` inside a string such as a URL.
    const lc = out.search(/(^|[^:"'`])\/\//);
    if (lc !== -1) out = out.slice(0, lc + 1);
    return out;
  });
}

let violations = 0;

for (const file of EDIT_PATH) {
  if (!existsSync(file)) {
    console.error(`check-exact-numeric: ${file} is missing; refusing to pass vacuously.`);
    process.exit(2);
  }
  codeLines(readFileSync(file, "utf8")).forEach((line, i) => {
    for (const [pattern, why] of FORBIDDEN) {
      if (pattern.test(line)) {
        violations++;
        console.error(`✗ ${file}:${i + 1}: ${why}\n    ${line.trim()}`);
      }
    }
  });
}

if (violations > 0) {
  console.error(`\ncheck-exact-numeric: ${violations} violation(s) on the exact-numeric edit path.`);
  process.exit(1);
}

console.log(`check-exact-numeric: ${EDIT_PATH.length} file(s) clean — no numeric conversion on the edit path.`);
