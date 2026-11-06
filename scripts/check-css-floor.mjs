#!/usr/bin/env bun
/**
 * D-023 / T-002: fail the build if the emitted CSS uses a feature above the
 * WebKitGTK 4.1 floor.
 *
 * D-019 makes WebKitGTK the compatibility floor for all UI work because Tauri
 * uses the OS webview and Linux is the awkward target. The classic failure it
 * names is discovering a broken grid on Linux late, so this turns "we targeted
 * the floor" from an intention into a check that runs on every build.
 *
 * Usage: bun run check:css   (after a build)
 */
import { readdirSync, readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

const DIST = "dist/assets";

/** Each entry: [pattern, why it is above the floor]. */
const FORBIDDEN = [
  [/\bsubgrid\b/, "CSS subgrid — named explicitly in D-023"],
  [/color-mix\s*\(/, "color-mix() — the reason we pin Tailwind v3, not v4"],
  [/@property\b/, "@property — registered custom properties are above the floor"],
  [/\boklch\s*\(/, "oklch() colour space"],
  [/\boklab\s*\(/, "oklab() colour space"],
  [/:has\s*\(/, ":has() — support is uneven at the floor"],
  [/@container\b/, "container queries"],
  [/text-wrap\s*:\s*balance/, "text-wrap: balance"],
  [/\banchor-name\b/, "CSS anchor positioning"],
];

if (!existsSync(DIST)) {
  console.error(`check-css-floor: ${DIST} not found — run the build first.`);
  process.exit(2);
}

const files = readdirSync(DIST).filter((f) => f.endsWith(".css"));
if (files.length === 0) {
  console.error("check-css-floor: no CSS emitted; refusing to pass vacuously.");
  process.exit(2);
}

let violations = 0;
for (const file of files) {
  const css = readFileSync(join(DIST, file), "utf8");
  for (const [pattern, why] of FORBIDDEN) {
    const m = css.match(pattern);
    if (m) {
      violations++;
      console.error(`✗ ${file}: ${why}\n    matched: ${JSON.stringify(m[0])}`);
    }
  }
}

if (violations > 0) {
  console.error(`\ncheck-css-floor: ${violations} violation(s) above the WebKitGTK 4.1 floor.`);
  process.exit(1);
}

console.log(`check-css-floor: ${files.length} file(s) clean against the WebKitGTK 4.1 floor.`);
