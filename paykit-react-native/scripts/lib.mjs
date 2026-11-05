// Paths and helpers shared by the scripts in this directory.
import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import yaml from 'js-yaml';

export const packageDir = path.dirname(
  path.dirname(fileURLToPath(import.meta.url))
);
export const repoRoot = path.dirname(packageDir);
// Where cargo, and so ubrn, puts the Rust libraries.
export const targetDir = process.env.CARGO_TARGET_DIR
  ? path.resolve(repoRoot, process.env.CARGO_TARGET_DIR)
  : path.join(repoRoot, 'target');
// Written by `ubrn build ios`; `vendored_frameworks` in the podspec.
export const XCFRAMEWORK = 'SynonymdevReactNativePaykitFramework.xcframework';

/** ubrn.config.yaml, the one list of iOS targets and Android ABIs. */
export function ubrnConfig() {
  return yaml.load(
    readFileSync(path.join(packageDir, 'ubrn.config.yaml'), 'utf8')
  );
}

/**
 * Run a command and throw if it fails. With `capture`, return its stdout;
 * otherwise it writes to the terminal. stderr always goes to the terminal.
 * `env` replaces the environment when given.
 */
export function run(
  command,
  args,
  { cwd = packageDir, capture = false, env = process.env } = {}
) {
  const result = spawnSync(command, args, {
    cwd,
    env,
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
    stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit',
  });
  if (result.status !== 0) {
    if (capture && result.stdout) {
      process.stderr.write(result.stdout);
    }
    const reason =
      result.error?.message ?? `exit ${result.status ?? result.signal}`;
    throw new Error(`${command} ${args.join(' ')} failed (${reason})`);
  }
  return result.stdout;
}

/**
 * Build lib/, then `npm pack` this package and return npm's JSON entry for
 * it. Scripts are skipped so that stdout is only the JSON.
 */
export function packJson(extraArgs = []) {
  run('npx', ['bob', 'build']);
  const [pack] = JSON.parse(
    run('npm', ['pack', '--json', '--ignore-scripts', ...extraArgs], {
      capture: true,
    })
  );
  return pack;
}
