// Cross-checks the class names used in components against the generated CSS.
//
// A Tailwind class that does not resolve produces no rule, so the element is
// silently unstyled rather than failing the build. That is exactly the kind of
// regression a token refactor can introduce, so it is asserted here instead of
// being discovered visually.
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

const SRC = "src";
const ASSETS = "dist/assets";

function walk(dir) {
  const out = [];
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) out.push(...walk(full));
    else if (/\.tsx?$/.test(entry)) out.push(full);
  }
  return out;
}

const css = readdirSync(ASSETS)
  .filter((name) => name.endsWith(".css"))
  .map((name) => readFileSync(join(ASSETS, name), "utf8"))
  .join("\n");

// Tailwind escapes the characters that are special in a selector, so `gap-1.5`
// becomes `.gap-1\.5` and `hover:bg-hover` becomes `.hover\:bg-hover:hover`.
function selectorFor(name) {
  return `.${name.replace(/[!"#$%&'()*+,./:;<=>?@[\\\]^`{|}~]/g, "\\$&")}`;
}

const used = new Set();
for (const file of walk(SRC)) {
  const text = readFileSync(file, "utf8");
  for (const match of text.matchAll(/className=(?:"([^"]*)"|\{`([^`]*)`\})/g)) {
    const raw = match[1] ?? match[2] ?? "";
    // Drop template interpolation; only static class strings are checkable.
    for (const token of raw.replace(/\$\{[^}]*\}/g, " ").split(/\s+/)) {
      if (token) used.add(token);
    }
  }
}

// Tokens that legitimately have no CSS rule of their own.
const IGNORED = new Set([
  "group",
  "peer",
  "container",
  "sr-only",
  "scroll-area",
  "label-mono",
  "code",
]);

const missing = [];
for (const name of used) {
  if (IGNORED.has(name)) continue;
  // Only static names; interpolated fragments were already stripped.
  if (name.includes("$") || name.includes("{")) continue;
  if (css.includes(selectorFor(name))) continue;
  missing.push(name);
}

if (missing.length > 0) {
  console.error(`MISSING (${missing.length} of ${used.size}):`);
  for (const name of [...new Set(missing)].sort()) console.error(`  ${name}`);
  process.exit(1);
}
console.log(`OK: all ${used.size} static class names resolve in the generated CSS`);
