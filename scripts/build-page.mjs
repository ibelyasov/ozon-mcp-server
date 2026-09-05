import { execFileSync } from "node:child_process";
import {
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const outputPath = join(root, "rust", "page.js");
const buildDirectory = mkdtempSync(join(tmpdir(), "ozon-page-"));
const check = process.argv.includes("--check");

try {
  execFileSync(join(root, "node_modules", ".bin", "tsc"), [
    "--project",
    join(root, "tsconfig.json"),
    "--outDir",
    buildDirectory,
    "--pretty",
    "false",
  ], { stdio: "inherit" });

  const compilerOutput = readFileSync(join(buildDirectory, "page.js"), "utf8");
  const compiled = compilerOutput
    .replace(/^"use strict";\r?\n/, "")
    .trimEnd();
  const artifact = `// Generated from rust/page.ts; run npm run build:page.\n(()=>{\n${compiled}\nreturn ozonPage;\n})()\n`;

  if (check) {
    const checkedIn = readFileSync(outputPath, "utf8");
    if (checkedIn !== artifact) {
      console.error("rust/page.js is stale; run npm run build:page");
      process.exitCode = 1;
    }
  } else {
    writeFileSync(outputPath, artifact);
  }
} finally {
  rmSync(buildDirectory, { recursive: true, force: true });
}
