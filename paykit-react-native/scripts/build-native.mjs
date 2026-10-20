// Build the native Paykit libraries that the npm package ships, and generate
// the bindings that go with them.
//
//   node scripts/build-native.mjs [ios] [android]   (default: both)
//
// This replaces `ubrn build`, which looks for `libpaykit_ffi` instead of
// `libpaykit` (see ubrn.config.yaml). Builds run from the workspace root, so
// they share ../target with paykit-ffi/build.sh and pick up the Android
// linker flags in ../.cargo/config.toml.
import { spawnSync } from 'node:child_process';
import { mkdirSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  ANDROID_ABIS,
  JNI_INIT_SYMBOL,
  findReadelf,
  verifyAndroidLibrary,
} from './android-elf.mjs';

const packageDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const repoRoot = path.join(packageDir, '..');
const targetDir = process.env.CARGO_TARGET_DIR ?? path.join(repoRoot, 'target');

// Same as paykit-ffi/build_ios.sh.
const IOS_DEPLOYMENT_TARGET = '13.4';
// Same as the targets in ubrn.config.yaml.
const IOS_TARGETS = ['aarch64-apple-ios', 'aarch64-apple-ios-sim'];
// `vendored_frameworks` in ReactNativePaykit.podspec.
const XCFRAMEWORK = path.join(
  packageDir,
  'SynonymdevReactNativePaykitFramework.xcframework'
);
// Read by android/CMakeLists.txt and android/build.gradle.
const JNI_LIBS = path.join(packageDir, 'android', 'src', 'main', 'jniLibs');
const VERIFIER_JAR = path.join(
  packageDir,
  'android',
  'libs',
  'rustls-platform-verifier.jar'
);
const CARGO_RELEASE = ['--release', '--locked', '-p', 'paykit-ffi'];

function run(command, args, options = {}) {
  console.log(`$ ${command} ${args.join(' ')}`);
  const result = spawnSync(command, args, {
    cwd: repoRoot,
    stdio: 'inherit',
    ...options,
  });
  if (result.status !== 0) {
    console.error(`${command} failed (${result.error ?? result.status})`);
    process.exit(result.status ?? 1);
  }
  return result;
}

function timed(label, step) {
  const start = Date.now();
  step();
  const seconds = ((Date.now() - start) / 1000).toFixed(1);
  console.log(`-- ${label}: ${seconds} s`);
}

function buildIos() {
  const env = {
    ...process.env,
    IPHONEOS_DEPLOYMENT_TARGET: IOS_DEPLOYMENT_TARGET,
  };
  for (const target of IOS_TARGETS) {
    run('cargo', ['build', ...CARGO_RELEASE, '--target', target], { env });
  }
  rmSync(XCFRAMEWORK, { recursive: true, force: true });
  run('xcodebuild', [
    '-create-xcframework',
    ...IOS_TARGETS.flatMap((target) => [
      '-library',
      path.join(targetDir, target, 'release', 'libpaykit.a'),
    ]),
    '-output',
    XCFRAMEWORK,
  ]);
}

// The Kotlin classes rustls-platform-verifier calls through JNI ship as an
// AAR inside the rustls-platform-verifier-android crate (patched to
// ../vendor). Locate it the same way paykit-ffi's Android Gradle build does.
function extractVerifierJar() {
  const metadata = run(
    'cargo',
    [
      'metadata',
      '--format-version',
      '1',
      '--locked',
      '--filter-platform',
      'aarch64-linux-android',
    ],
    { stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 256 * 1024 * 1024 }
  );
  const verifier = JSON.parse(metadata.stdout).packages.find(
    (pkg) => pkg.name === 'rustls-platform-verifier-android'
  );
  if (!verifier) {
    throw new Error('rustls-platform-verifier-android not in cargo metadata');
  }
  const aar = path.join(
    path.dirname(verifier.manifest_path),
    'maven/rustls/rustls-platform-verifier',
    verifier.version,
    `rustls-platform-verifier-${verifier.version}.aar`
  );
  const jar = run('unzip', ['-p', aar, 'classes.jar'], {
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  mkdirSync(path.dirname(VERIFIER_JAR), { recursive: true });
  writeFileSync(VERIFIER_JAR, jar.stdout);
  console.log(`Wrote ${VERIFIER_JAR} from ${aar}`);
}

function buildAndroid() {
  rmSync(JNI_LIBS, { recursive: true, force: true });
  run('cargo', [
    'ndk',
    '-o',
    JNI_LIBS,
    ...ANDROID_ABIS.flatMap((abi) => ['-t', abi]),
    'build',
    ...CARGO_RELEASE,
  ]);
  const readelf = findReadelf();
  for (const abi of ANDROID_ABIS) {
    const dir = path.join(JNI_LIBS, abi);
    // cargo-ndk copies every shared library it finds; keep only ours, as
    // paykit-ffi/build_android.sh does.
    for (const file of readdirSync(dir)) {
      if (file !== 'libpaykit.so') {
        rmSync(path.join(dir, file));
      }
    }
    verifyAndroidLibrary(
      readelf,
      path.join(dir, 'libpaykit.so'),
      JNI_INIT_SYMBOL
    );
  }
  extractVerifierJar();
}

const builders = { ios: buildIos, android: buildAndroid };
const platforms =
  process.argv.length > 2 ? process.argv.slice(2) : ['ios', 'android'];
for (const platform of platforms) {
  if (!builders[platform]) {
    console.error(`Unknown platform '${platform}'. Use ios and/or android.`);
    process.exit(2);
  }
}

timed('host library and bindings', () => {
  run('cargo', ['build', ...CARGO_RELEASE]);
  run(
    process.execPath,
    [path.join(packageDir, 'scripts', 'ubrn-generate.mjs')],
    {
      cwd: packageDir,
    }
  );
});
for (const platform of platforms) {
  timed(platform, builders[platform]);
}
