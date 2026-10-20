// Generate the TypeScript and C++ bindings plus the TurboModule glue from a
// host build of paykit-ffi. Run `cargo build --release -p paykit-ffi` first.
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const packageDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const targetDir =
  process.env.CARGO_TARGET_DIR ?? path.join(packageDir, '..', 'target');
const extension = process.platform === 'darwin' ? 'dylib' : 'so';
const library = path.join(targetDir, 'release', `libpaykit.${extension}`);
// ubrn runs `cargo metadata` against ./Cargo.toml, so bindings are generated
// from the crate directory.
const crateDir = path.join(packageDir, '..', 'paykit-ffi');

if (!existsSync(library)) {
  console.error(
    `Missing ${library}. Run \`cargo build --release -p paykit-ffi\` first.`
  );
  process.exit(1);
}

function ubrn(cwd, ...args) {
  const result = spawnSync('ubrn', args, { cwd, stdio: 'inherit' });
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

ubrn(
  crateDir,
  'generate',
  'jsi',
  'bindings',
  '--library',
  '--ts-dir',
  path.join(packageDir, 'src', 'generated'),
  '--cpp-dir',
  path.join(packageDir, 'cpp', 'generated'),
  library
);
ubrn(
  packageDir,
  'generate',
  'jsi',
  'turbo-module',
  '--config',
  'ubrn.config.yaml',
  'paykit'
);
