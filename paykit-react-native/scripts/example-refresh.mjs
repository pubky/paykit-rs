// Pack this package and install the tarball into example/, so the example app
// consumes exactly what npm would publish: the `files` list, the prebuilt
// native libraries, the podspec and Gradle autolinking. Run
// `npm run build:native` first.
//
// The tarball keeps a fixed name, so the dependency spec committed in
// example/package.json never changes. The tarball itself is gitignored.
import { mkdirSync, renameSync, rmSync } from 'node:fs';
import path from 'node:path';
import { packJson, packageDir, run } from './lib.mjs';

const exampleDir = path.join(packageDir, 'example');
const tarballDir = path.join(exampleDir, 'tarballs');
const EXAMPLE_TARBALL = 'synonymdev-react-native-paykit.tgz';

rmSync(tarballDir, { recursive: true, force: true });
mkdirSync(tarballDir, { recursive: true });

const pack = packJson(['--pack-destination', tarballDir]);
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
run('npm', ['install', '--no-audit', '--no-fund'], { cwd: exampleDir });
console.log(`installed ${EXAMPLE_TARBALL} into example/node_modules`);
