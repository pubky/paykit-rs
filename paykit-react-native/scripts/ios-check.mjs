// Compile-check the iOS side of the package without an app:
// 1. the XCFramework has a device and a simulator slice;
// 2. the podspec parses with React Native's CocoaPods helpers loaded, the way
//    an app's Podfile loads it;
// 3. the C++ glue compiles for the simulator, and every Rust symbol it calls
//    is defined in the simulator libpaykit.a.
// Run `npm run build:ios` first.
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const packageDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const require = createRequire(import.meta.url);
const XCFRAMEWORK = 'SynonymdevReactNativePaykitFramework.xcframework';
const SLICES = ['ios-arm64', 'ios-arm64-simulator'];

function output(command, args, options = {}) {
  return execFileSync(command, args, {
    cwd: packageDir,
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
    ...options,
  });
}

function packageRoot(name) {
  return path.dirname(require.resolve(`${name}/package.json`));
}

for (const slice of SLICES) {
  const lib = path.join(packageDir, XCFRAMEWORK, slice, 'libpaykit.a');
  if (!existsSync(lib)) {
    throw new Error(`Missing ${lib}`);
  }
  console.log(`OK ${slice}: ${output('lipo', ['-archs', lib]).trim()}`);
}

const reactNative = packageRoot('react-native');
const spec = JSON.parse(
  output('pod', ['ipc', 'spec', 'ReactNativePaykit.podspec'], {
    env: {
      ...process.env,
      RUBYOPT: `-r${path.join(reactNative, 'scripts', 'react_native_pods.rb')}`,
    },
  })
);
const { version } = JSON.parse(
  readFileSync(path.join(packageDir, 'package.json'), 'utf8')
);
if (
  spec.vendored_frameworks !== XCFRAMEWORK ||
  spec.source.tag !== `v${version}`
) {
  throw new Error(
    `Unexpected podspec: ${JSON.stringify({ frameworks: spec.vendored_frameworks, source: spec.source })}`
  );
}
console.log(`OK podspec ${spec.name} ${spec.version}, tag ${spec.source.tag}`);

const objects = mkdtempSync(path.join(os.tmpdir(), 'paykit-ios-check-'));
try {
  const ubrnIncludes = path.join(
    packageRoot('uniffi-bindgen-react-native'),
    'cpp',
    'includes'
  );
  const reactCommon = path.join(reactNative, 'ReactCommon');
  const undefinedSymbols = new Set();
  for (const source of [
    'cpp/synonymdev-react-native-paykit.cpp',
    'cpp/generated/paykit.cpp',
  ]) {
    const object = path.join(objects, `${path.basename(source)}.o`);
    output('xcrun', [
      '--sdk',
      'iphonesimulator',
      'clang++',
      '-std=c++20',
      '-target',
      `arm64-apple-ios${spec.platforms.ios}-simulator`,
      '-Werror',
      `-I${path.join(packageDir, 'cpp')}`,
      `-I${ubrnIncludes}`,
      `-I${path.join(reactCommon, 'jsi')}`,
      `-I${path.join(reactCommon, 'callinvoker')}`,
      `-I${reactCommon}`,
      '-c',
      source,
      '-o',
      object,
    ]);
    for (const symbol of output('nm', ['-u', '-j', object]).split('\n')) {
      if (symbol.startsWith('_uniffi_') || symbol.startsWith('_ffi_')) {
        undefinedSymbols.add(symbol);
      }
    }
  }
  const simulatorLib = path.join(
    packageDir,
    XCFRAMEWORK,
    'ios-arm64-simulator',
    'libpaykit.a'
  );
  // Xcode's nm cannot read some LLVM bitcode members that rustc puts in the
  // archive (compiler_builtins) and exits non-zero, but still lists the rest.
  // Ignoring the status fails closed: unread members only shrink this set.
  const defined = new Set(
    spawnSync('nm', ['-g', '-j', '--defined-only', simulatorLib], {
      encoding: 'utf8',
      maxBuffer: 256 * 1024 * 1024,
    }).stdout.split('\n')
  );
  const missing = [...undefinedSymbols].filter((s) => !defined.has(s));
  if (undefinedSymbols.size === 0 || missing.length > 0) {
    throw new Error(
      `libpaykit.a lacks ${missing.length} of ${undefinedSymbols.size} symbols the C++ glue calls: ${missing.slice(0, 10).join(', ')}`
    );
  }
  console.log(
    `OK C++ glue compiles; libpaykit.a defines all ${undefinedSymbols.size} Rust symbols it calls`
  );
} finally {
  rmSync(objects, { recursive: true, force: true });
}
