// Runs after `ubrn build android` (the `postbuild:android` npm script) and
// does what ubrn does not:
// 1. writes android/libs/rustls-platform-verifier.jar, the Kotlin classes
//    that rustls-platform-verifier calls through JNI;
// 2. strips every libpaykit.so that ubrn copied into android/src/main/jniLibs,
//    as cargo-ndk does when it copies libraries itself (ubrn copies them, so
//    the libraries in ../target keep the symbols ubrn generates bindings
//    from), and checks them with the same ELF checks that
//    paykit-ffi/build_android.sh runs.
//
// It also exports the ELF checks for android-assemble.mjs.
import { existsSync, readdirSync, renameSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { packageDir, repoRoot, run } from './lib.mjs';

// Exported by libpaykit.so (paykit-ffi/src/lib.rs) and bound by
// android/src/main/java/com/synonym/paykit/PaykitAndroid.kt.
export const JNI_INIT_SYMBOL =
  'Java_com_synonym_paykit_PaykitAndroid_nativeInitialize';

const PAGE_SIZE_16_KB = 16384;
// Read by android/build.gradle.
const JNI_LIBS = path.join(packageDir, 'android', 'src', 'main', 'jniLibs');
const VERIFIER_JAR = path.join(
  packageDir,
  'android',
  'libs',
  'rustls-platform-verifier.jar'
);

function onPath(name) {
  for (const dir of (process.env.PATH ?? '').split(path.delimiter)) {
    const candidate = path.join(dir, name);
    if (dir && existsSync(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

function inNdk(tool) {
  for (const variable of ['ANDROID_NDK_ROOT', 'ANDROID_NDK_HOME', 'NDK_HOME']) {
    const prebuilt = path.join(
      process.env[variable] ?? '',
      'toolchains',
      'llvm',
      'prebuilt'
    );
    if (!process.env[variable] || !existsSync(prebuilt)) {
      continue;
    }
    for (const host of readdirSync(prebuilt)) {
      const candidate = path.join(prebuilt, host, 'bin', tool);
      if (existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return undefined;
}

export function findReadelf() {
  const readelf =
    onPath('llvm-readelf') ?? onPath('readelf') ?? inNdk('llvm-readelf');
  if (!readelf) {
    throw new Error(
      'llvm-readelf or readelf is required. Put one on PATH or set ANDROID_NDK_HOME.'
    );
  }
  return readelf;
}

// Throws unless every LOAD segment of `lib` is aligned to at least 16 KB
// (required by Android 15+ devices with 16 KB pages) and, when given,
// `exportedSymbol` is in its dynamic symbol table.
export function verifyAndroidLibrary(readelf, lib, exportedSymbol) {
  const alignments = run(readelf, ['-W', '-l', lib], { capture: true })
    .split('\n')
    .map((line) => line.trim().split(/\s+/))
    .filter((fields) => fields[0] === 'LOAD')
    .map((fields) => Number(fields[fields.length - 1]));
  if (
    alignments.length === 0 ||
    alignments.some((alignment) => !(alignment >= PAGE_SIZE_16_KB))
  ) {
    throw new Error(
      `${lib} is not 16 KB page-size aligned (LOAD alignments: ${alignments.join(', ')})`
    );
  }
  if (
    exportedSymbol &&
    !run(readelf, ['-W', '--dyn-syms', lib], { capture: true }).includes(
      exportedSymbol
    )
  ) {
    throw new Error(`${lib} does not export ${exportedSymbol}`);
  }
  console.log(`OK ${lib}: LOAD alignment ${Math.min(...alignments)}`);
}

// The classes ship as an AAR inside the rustls-platform-verifier-android
// crate (patched to ../vendor). Locate it the same way paykit-ffi's Android
// Gradle build does.
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
    { cwd: repoRoot, capture: true }
  );
  const verifier = JSON.parse(metadata).packages.find(
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
  const libs = path.dirname(VERIFIER_JAR);
  run('unzip', ['-o', '-q', aar, 'classes.jar', '-d', libs]);
  renameSync(path.join(libs, 'classes.jar'), VERIFIER_JAR);
  console.log(`Wrote ${VERIFIER_JAR} from ${aar}`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  extractVerifierJar();
  // ubrn clears jniLibs, then copies in one libpaykit.so per ABI it built.
  const abis = existsSync(JNI_LIBS) ? readdirSync(JNI_LIBS) : [];
  if (abis.length === 0) {
    throw new Error(`No libraries in ${JNI_LIBS}`);
  }
  const strip = inNdk('llvm-strip') ?? onPath('llvm-strip');
  if (!strip) {
    throw new Error('llvm-strip is required. Set ANDROID_NDK_HOME.');
  }
  const readelf = findReadelf();
  for (const abi of abis) {
    const lib = path.join(JNI_LIBS, abi, 'libpaykit.so');
    run(strip, [lib]);
    verifyAndroidLibrary(readelf, lib, JNI_INIT_SYMBOL);
  }
}
