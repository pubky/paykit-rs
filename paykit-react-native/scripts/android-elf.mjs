// ELF checks for the Android native libraries, the same checks that
// paykit-ffi/build_android.sh runs on the Kotlin package's libraries.
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync } from 'node:fs';
import path from 'node:path';

// Exported by libpaykit.so (paykit-ffi/src/lib.rs) and bound by
// android/src/main/java/com/synonym/paykit/PaykitAndroid.kt.
export const JNI_INIT_SYMBOL =
  'Java_com_synonym_paykit_PaykitAndroid_nativeInitialize';

// Same as the Android targets in ubrn.config.yaml.
export const ANDROID_ABIS = ['arm64-v8a', 'armeabi-v7a', 'x86_64', 'x86'];

const PAGE_SIZE_16_KB = 16384;

function onPath(name) {
  for (const dir of (process.env.PATH ?? '').split(path.delimiter)) {
    const candidate = path.join(dir, name);
    if (dir && existsSync(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

function inNdk() {
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
      const candidate = path.join(prebuilt, host, 'bin', 'llvm-readelf');
      if (existsSync(candidate)) {
        return candidate;
      }
    }
  }
  return undefined;
}

export function findReadelf() {
  const readelf = onPath('llvm-readelf') ?? onPath('readelf') ?? inNdk();
  if (!readelf) {
    throw new Error(
      'llvm-readelf or readelf is required. Put one on PATH or set ANDROID_NDK_HOME.'
    );
  }
  return readelf;
}

function readelfOutput(readelf, ...args) {
  return execFileSync(readelf, args, {
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
  });
}

// Throws unless every LOAD segment of `lib` is aligned to at least 16 KB
// (required by Android 15+ devices with 16 KB pages) and, when given,
// `exportedSymbol` is in its dynamic symbol table.
export function verifyAndroidLibrary(readelf, lib, exportedSymbol) {
  const alignments = readelfOutput(readelf, '-W', '-l', lib)
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
    !readelfOutput(readelf, '-W', '--dyn-syms', lib).includes(exportedSymbol)
  ) {
    throw new Error(`${lib} does not export ${exportedSymbol}`);
  }
  console.log(`OK ${lib}: LOAD alignment ${Math.min(...alignments)}`);
}
