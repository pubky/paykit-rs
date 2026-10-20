// Pack this package and install the tarball into example/, so the example app
// consumes exactly what npm would publish: the `files` list, the prebuilt
// native libraries, the podspec and Gradle autolinking. Run
// `npm run build:native` first.
//
// The tarball keeps a fixed name, so the dependency spec committed in
// example/package.json never changes. The tarball itself is gitignored.
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { packJson, packageDir, run } from './lib.mjs';

const exampleDir = path.join(packageDir, 'example');
const tarballDir = path.join(exampleDir, 'tarballs');
const EXAMPLE_TARBALL = 'synonymdev-react-native-paykit.tgz';
const tarballPath = path.join(tarballDir, EXAMPLE_TARBALL);
const lockPath = path.join(exampleDir, 'package-lock.json');
const installedDir = path.join(
  exampleDir,
  'node_modules',
  '@synonymdev',
  'react-native-paykit'
);
const LOCK_KEY = 'node_modules/@synonymdev/react-native-paykit';
// Files compared between the tarball and the installed copy: package.json and
// the first native library the tarball contains (an iOS-only or Android-only
// build packs only one of them).
const CHECKED_NATIVE_FILES = [
  'android/src/main/jniLibs/arm64-v8a/libpaykit.so',
  'SynonymdevReactNativePaykitFramework.xcframework/ios-arm64/libpaykit.a',
];

/**
 * Drop the lockfile `integrity` of the tarball dependency. With an integrity
 * and a `resolved` path, npm (pacote) reads the tarball from its cache by that
 * hash instead of from the file, so after a native rebuild it would install
 * the previous tarball's content. Without it npm reads the file. The tarball
 * changes on every rebuild, so its hash does not belong in the committed lock.
 */
function dropTarballIntegrity() {
  const lock = JSON.parse(readFileSync(lockPath, 'utf8'));
  const entry = lock.packages?.[LOCK_KEY];
  if (entry && 'integrity' in entry) {
    delete entry.integrity;
    writeFileSync(lockPath, `${JSON.stringify(lock, null, 2)}\n`);
  }
}

/** Fail unless the installed package matches the tarball byte for byte. */
function assertInstalledMatchesTarball() {
  const listing = run('tar', ['-tzf', tarballPath], { capture: true })
    .split('\n')
    .map((entry) => entry.replace(/^package\//, ''));
  const native = CHECKED_NATIVE_FILES.find((file) => listing.includes(file));
  if (!native) {
    throw new Error(`${EXAMPLE_TARBALL} contains no native library`);
  }
  const checked = ['package.json', native];
  const extractDir = mkdtempSync(path.join(os.tmpdir(), 'paykit-tarball-'));
  try {
    run('tar', [
      '-xzf',
      tarballPath,
      '-C',
      extractDir,
      ...checked.map((file) => `package/${file}`),
    ]);
    for (const file of checked) {
      const installed = path.join(installedDir, file);
      if (
        !existsSync(installed) ||
        !readFileSync(installed).equals(
          readFileSync(path.join(extractDir, 'package', file))
        )
      ) {
        throw new Error(
          `example/node_modules has a stale ${file}: it differs from ` +
            `${EXAMPLE_TARBALL}. npm installed an older package.`
        );
      }
    }
  } finally {
    rmSync(extractDir, { recursive: true, force: true });
  }
  return checked;
}

rmSync(tarballDir, { recursive: true, force: true });
mkdirSync(tarballDir, { recursive: true });

const pack = packJson(['--pack-destination', tarballDir]);
renameSync(path.join(tarballDir, pack.filename), tarballPath);
console.log(
  `packed ${pack.name}@${pack.version}: ${(pack.size / 1024 / 1024).toFixed(1)} MB`
);

// Remove the installed copy first: npm keeps an installed `file:` tarball
// dependency when the lockfile entry still matches its spec.
rmSync(path.join(exampleDir, 'node_modules', '@synonymdev'), {
  recursive: true,
  force: true,
});
dropTarballIntegrity();
run('npm', ['install', '--no-audit', '--no-fund'], { cwd: exampleDir });
// npm records the new tarball's hash; drop it again so the committed lock
// stays unchanged.
dropTarballIntegrity();
const checked = assertInstalledMatchesTarball();
console.log(
  `installed ${EXAMPLE_TARBALL} into example/node_modules ` +
    `(verified ${checked.join(', ')})`
);
