// Check what `npm pack` would publish: the prebuilt iOS XCFramework and
// Android libraries, the C++ and TypeScript sources and the compiled lib/.
// Run `npm run build:native` first. Prints the file list and the sizes.
import { spawnSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ANDROID_ABIS } from './android-elf.mjs';

const packageDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const XCFRAMEWORK = 'SynonymdevReactNativePaykitFramework.xcframework';

const REQUIRED = [
  'package.json',
  'ReactNativePaykit.podspec',
  `${XCFRAMEWORK}/Info.plist`,
  `${XCFRAMEWORK}/ios-arm64/libpaykit.a`,
  `${XCFRAMEWORK}/ios-arm64-simulator/libpaykit.a`,
  ...ANDROID_ABIS.map((abi) => `android/src/main/jniLibs/${abi}/libpaykit.so`),
  'android/libs/rustls-platform-verifier.jar',
  'android/build.gradle',
  'android/CMakeLists.txt',
  'android/proguard-rules.pro',
  'android/src/main/java/com/synonym/paykit/PaykitAndroid.kt',
  'cpp/synonymdev-react-native-paykit.cpp',
  'cpp/generated/paykit.cpp',
  'cpp/generated/paykit.hpp',
  'ios/ReactNativePaykit.mm',
  'src/index.tsx',
  'src/generated/paykit.ts',
  'lib/module/index.js',
  'lib/typescript/src/index.d.ts',
];
// Build output, tests and repository-only tooling.
const FORBIDDEN = [
  'android/build/',
  'android/.cxx/',
  'android-check/',
  'node_modules/',
  'scripts/',
  'src/__tests__/',
];

function run(command, args) {
  const result = spawnSync(command, args, {
    cwd: packageDir,
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

// Build lib/ first, then pack without scripts so stdout is only the JSON.
console.log(run('npx', ['bob', 'build']));
const [pack] = JSON.parse(
  run('npm', ['pack', '--dry-run', '--json', '--ignore-scripts'])
);
const files = pack.files.map((file) => file.path).sort();

for (const file of files) {
  console.log(file);
}
const megabytes = (bytes) => `${(bytes / 1024 / 1024).toFixed(1)} MB`;
console.log(
  `${pack.filename}: ${files.length} files, tarball ${megabytes(pack.size)}, unpacked ${megabytes(pack.unpackedSize)}`
);

const missing = REQUIRED.filter((file) => !files.includes(file));
const forbidden = files.filter((file) =>
  FORBIDDEN.some((prefix) => file.startsWith(prefix))
);
if (missing.length > 0 || forbidden.length > 0) {
  for (const file of missing) {
    console.error(`missing: ${file}`);
  }
  for (const file of forbidden) {
    console.error(`must not be packed: ${file}`);
  }
  process.exit(1);
}
