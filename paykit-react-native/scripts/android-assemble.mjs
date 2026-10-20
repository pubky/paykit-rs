// Assemble the Android library without an app (android-check/), then check
// the release AAR: it packages only the Paykit native libraries, both are
// 16 KB aligned for every ABI,
// libpaykit.so exports the TLS init symbol, and the PaykitAndroid and
// rustls-platform-verifier classes are packaged.
//
// Run `npm run build:android` first. Extra arguments go to Gradle, e.g.
//   npm run android:assemble -- -PReactNativePaykit_ndkVersion=27.1.12297006
import { existsSync, mkdtempSync, readdirSync, rmSync } from 'node:fs';
import { createRequire } from 'node:module';
import os from 'node:os';
import path from 'node:path';
import {
  JNI_INIT_SYMBOL,
  findReadelf,
  verifyAndroidLibrary,
} from './android-elf.mjs';
import { packageDir, run, ubrnConfig } from './lib.mjs';

const NATIVE_LIBRARIES = [
  'libpaykit.so',
  'libsynonymdev-react-native-paykit.so',
];
const PACKAGED_CLASSES = [
  'com/synonym/paykit/PaykitAndroid.class',
  'com/synonymdev/reactnativepaykit/ReactNativePaykitModule.class',
];

// The Gradle wrapper that ships with React Native's Gradle plugin, so the
// Gradle version always matches the React Native version.
const gradlew = path.join(
  path.dirname(
    createRequire(import.meta.url).resolve(
      '@react-native/gradle-plugin/package.json'
    )
  ),
  'gradlew'
);
run(gradlew, [
  '-p',
  path.join(packageDir, 'android-check'),
  ':synonymdev_react-native-paykit:assembleRelease',
  ...process.argv.slice(2),
]);

const aarDir = path.join(packageDir, 'android', 'build', 'outputs', 'aar');
const aar = readdirSync(aarDir).find((file) => file.endsWith('release.aar'));
if (!aar) {
  throw new Error(`No release AAR in ${aarDir}`);
}

const unpacked = mkdtempSync(path.join(os.tmpdir(), 'paykit-aar-'));
try {
  run('unzip', ['-q', path.join(aarDir, aar), '-d', unpacked]);
  const readelf = findReadelf();
  for (const abi of ubrnConfig().android.targets) {
    const jni = path.join(unpacked, 'jni', abi);
    // React Native's own libraries come from react-android in the app.
    const packaged = readdirSync(jni).sort().join(', ');
    if (packaged !== NATIVE_LIBRARIES.join(', ')) {
      throw new Error(`${aar} packages ${packaged} for ${abi}`);
    }
    verifyAndroidLibrary(
      readelf,
      path.join(jni, 'libpaykit.so'),
      JNI_INIT_SYMBOL
    );
    verifyAndroidLibrary(
      readelf,
      path.join(jni, 'libsynonymdev-react-native-paykit.so')
    );
  }

  // AGP packages local jar dependencies under libs/ in the AAR.
  const libs = path.join(unpacked, 'libs');
  const jars = [
    path.join(unpacked, 'classes.jar'),
    ...(existsSync(libs)
      ? readdirSync(libs).map((jar) => path.join(libs, jar))
      : []),
  ];
  const classes = jars
    .map((jar) => run('unzip', ['-Z1', jar], { capture: true }))
    .join('\n');
  const missing = PACKAGED_CLASSES.filter((name) => !classes.includes(name));
  if (!classes.includes('org/rustls/platformverifier/')) {
    missing.push('org/rustls/platformverifier/*');
  }
  if (missing.length > 0) {
    throw new Error(`${aar} is missing ${missing.join(', ')}`);
  }
  console.log(`OK ${aar}`);
} finally {
  rmSync(unpacked, { recursive: true, force: true });
}
