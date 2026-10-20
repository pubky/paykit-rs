// Generate the TypeScript and C++ bindings plus the TurboModule glue from a
// host build of paykit-ffi, without cross-compiling. CI uses it to type-check
// and to check the committed glue on Linux. Run
// `cargo build --release -p paykit-ffi` first.
//
// `npm run build:native` generates the same files from the iOS and Android
// libraries (`ubrn build --and-generate`).
import { existsSync } from 'node:fs';
import path from 'node:path';
import { packageDir, repoRoot, run, targetDir } from './lib.mjs';

const extension = process.platform === 'darwin' ? 'dylib' : 'so';
const library = path.join(targetDir, 'release', `libpaykit.${extension}`);

if (!existsSync(library)) {
  throw new Error(
    `Missing ${library}. Run \`cargo build --release -p paykit-ffi\` first.`
  );
}

// ubrn runs `cargo metadata` against ./Cargo.toml, so bindings are generated
// from the crate directory.
run(
  'ubrn',
  [
    'generate',
    'jsi',
    'bindings',
    '--library',
    '--ts-dir',
    path.join(packageDir, 'src', 'generated'),
    '--cpp-dir',
    path.join(packageDir, 'cpp', 'generated'),
    library,
  ],
  { cwd: path.join(repoRoot, 'paykit-ffi') }
);
run('ubrn', [
  'generate',
  'jsi',
  'turbo-module',
  '--config',
  'ubrn.config.yaml',
  'paykit',
]);
