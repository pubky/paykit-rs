// Pack this package and install the tarball into example/, so the example app
// consumes exactly what npm would publish: the `files` list, the prebuilt
// native libraries, the podspec and Gradle autolinking. Run
// `npm run build:native` first.
//
// The tarball keeps a fixed name, so the dependency spec committed in
// example/package.json never changes. The tarball itself is gitignored.
import { spawnSync } from 'node:child_process';
import { mkdirSync, renameSync, rmSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const packageDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const exampleDir = path.join(packageDir, 'example');
const tarballDir = path.join(exampleDir, 'tarballs');
const EXAMPLE_TARBALL = 'synonymdev-react-native-paykit.tgz';

function run(command, args, cwd) {
  const result = spawnSync(command, args, {
    cwd,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  if (result.status !== 0) {
    console.error(result.stdout);
    process.exit(result.status ?? 1);
  }
  return result.stdout;
}

rmSync(tarballDir, { recursive: true, force: true });
mkdirSync(tarballDir, { recursive: true });

// Build lib/ first, then pack without scripts so stdout is only the JSON.
run('npx', ['bob', 'build'], packageDir);
const [pack] = JSON.parse(
  run(
    'npm',
    ['pack', '--json', '--ignore-scripts', '--pack-destination', tarballDir],
    packageDir
  )
);
renameSync(
  path.join(tarballDir, pack.filename),
  path.join(tarballDir, EXAMPLE_TARBALL)
);
console.log(
  `packed ${pack.name}@${pack.version}: ${(pack.size / 1024 / 1024).toFixed(1)} MB`
);

// Remove the installed copy first: npm keeps an installed `file:` tarball
// dependency when the lockfile entry still matches its spec.
rmSync(path.join(exampleDir, 'node_modules', '@synonymdev'), {
  recursive: true,
  force: true,
});
run('npm', ['install', '--no-audit', '--no-fund'], exampleDir);
console.log(`installed ${EXAMPLE_TARBALL} into example/node_modules`);
