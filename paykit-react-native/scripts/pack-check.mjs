// Check what `npm pack` would publish: the prebuilt iOS XCFramework and
// Android libraries, the C++ and TypeScript sources and the compiled lib/.
// Run `npm run build:native` first. Prints the file list and the sizes.
import { XCFRAMEWORK, packJson, ubrnConfig } from './lib.mjs';

const REQUIRED = [
  'package.json',
  'ReactNativePaykit.podspec',
  `${XCFRAMEWORK}/Info.plist`,
  `${XCFRAMEWORK}/ios-arm64/libpaykit.a`,
  `${XCFRAMEWORK}/ios-arm64-simulator/libpaykit.a`,
  ...ubrnConfig().android.targets.map(
    (abi) => `android/src/main/jniLibs/${abi}/libpaykit.so`
  ),
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
  'example/',
  'node_modules/',
  'scripts/',
  'src/__tests__/',
];

const pack = packJson(['--dry-run']);
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
