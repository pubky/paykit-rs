// Test rig code: prints the sizes of harness bundles built by build.sh. It prints sizes only and
// enforces no budget.
//
//   node paykit-browser/rig/harness/size.mjs <label>=<path to .wasm> [<label>=<path> ...]
//
// For each bundle it prints, as one row of a Markdown table, the .wasm file's size in bytes:
// raw, gzip (level 9) and brotli (quality 11, given the input size as a hint), as Node's zlib
// compresses them; and the size of the JavaScript glue beside it (<name>.js for
// <name>_bg.wasm, as wasm-bindgen names them, so harness.js for harness_bg.wasm), raw and
// gzip, or "-" when there is none. When GITHUB_STEP_SUMMARY is set, it also appends the table
// to that file, so it shows on the run's summary page.

import { appendFileSync, existsSync, readFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { brotliCompressSync, constants, gzipSync } from "node:zlib";

function usage(message) {
  if (message) console.error(`error: ${message}`);
  console.error("usage: size.mjs <label>=<path to .wasm> [<label>=<path to .wasm> ...]");
  process.exit(2);
}

function gzipBytes(bytes) {
  return gzipSync(bytes, { level: 9 }).length;
}

function brotliBytes(bytes) {
  return brotliCompressSync(bytes, {
    params: {
      [constants.BROTLI_PARAM_QUALITY]: 11,
      [constants.BROTLI_PARAM_SIZE_HINT]: bytes.length,
    },
  }).length;
}

// The glue wasm-bindgen writes beside <name>_bg.wasm, or undefined when there is none.
function gluePath(wasmPath) {
  const name = basename(wasmPath);
  if (!name.endsWith("_bg.wasm")) return undefined;
  const glue = join(dirname(wasmPath), `${name.slice(0, -"_bg.wasm".length)}.js`);
  return existsSync(glue) ? glue : undefined;
}

function row(argument) {
  const at = argument.indexOf("=");
  if (at <= 0 || at === argument.length - 1) usage(`"${argument}" is not <label>=<path to .wasm>`);
  const label = argument.slice(0, at);
  const path = argument.slice(at + 1);
  if (!path.endsWith(".wasm")) usage(`${path} is not a .wasm file`);
  if (!existsSync(path)) usage(`${path} does not exist`);
  const wasm = readFileSync(path);
  const glue = gluePath(path);
  const glueBytes = glue === undefined ? undefined : readFileSync(glue);
  return [
    label,
    String(wasm.length),
    String(gzipBytes(wasm)),
    String(brotliBytes(wasm)),
    glueBytes === undefined ? "-" : String(glueBytes.length),
    glueBytes === undefined ? "-" : String(gzipBytes(glueBytes)),
  ];
}

const args = process.argv.slice(2);
if (args.length === 0) usage();
const header = ["bundle", "wasm raw", "wasm gzip", "wasm brotli", "glue raw", "glue gzip"];
const rows = args.map(row);
const line = (cells) => `| ${cells.join(" | ")} |`;
const table = [
  "Harness bundle sizes in bytes (gzip level 9, brotli quality 11):",
  "",
  line(header),
  line(header.map((_, i) => (i === 0 ? "---" : "---:"))),
  ...rows.map(line),
  "",
].join("\n");
process.stdout.write(table);
if (process.env.GITHUB_STEP_SUMMARY) {
  appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${table}\n`);
}
