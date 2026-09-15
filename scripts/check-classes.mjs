// Cross-check every className used in the TSX against the built stylesheet.
// Reports classes with no rule (broken styling) and rules with no usage
// (dead CSS). Read-only.
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const srcDir = "src";
const stylesDir = "src/styles";

const tsxFiles = readdirSync(srcDir).filter((f) => f.endsWith(".tsx"));
const cssFiles = readdirSync(stylesDir).filter((f) => f.endsWith(".css"));

const used = new Set();
for (const file of tsxFiles) {
  const text = readFileSync(join(srcDir, file), "utf8");
  // className="a b" and className={`a${cond ? " b" : ""}`}
  for (const m of text.matchAll(/className=(?:"([^"]*)"|\{`([^`]*)`\})/g)) {
    const raw = m[1] ?? m[2] ?? "";
    // Strip template interpolation, keep literal fragments.
    const cleaned = raw.replace(/\$\{[^}]*\}/g, " ");
    for (const token of cleaned.split(/\s+/)) {
      const t = token.trim();
      if (t) used.add(t);
    }
  }
}

const defined = new Set();
for (const file of cssFiles) {
  const text = readFileSync(join(stylesDir, file), "utf8");
  for (const m of text.matchAll(/\.([a-zA-Z][\w-]*)/g)) {
    defined.add(m[1]);
  }
}

// Classes applied conditionally through helper functions in TSX.
const dynamic = ["active", "show", "mono", "absent"];

const missing = [...used].filter((c) => !defined.has(c)).sort();
const unused = [...defined]
  .filter((c) => !used.has(c) && !dynamic.includes(c))
  .sort();

console.log("used classes:", used.size);
console.log("defined classes:", defined.size);
console.log("\n[MISSING] used in TSX but no CSS rule:");
console.log(missing.length ? missing.join("\n") : "  (none)");
console.log("\n[UNUSED] CSS rule with no TSX usage:");
console.log(unused.length ? unused.join("\n") : "  (none)");
