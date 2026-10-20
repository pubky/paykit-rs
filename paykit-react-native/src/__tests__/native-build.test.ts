import { readFileSync } from 'node:fs';
import path from 'node:path';

// Static checks on the hand-edited native glue and build scripts. They catch
// drift that only shows up on a device or in a full native build.

const packageDir = path.join(__dirname, '..', '..');
const repoRoot = path.join(packageDir, '..');

function read(...segments: string[]): string {
  return readFileSync(path.join(...segments), 'utf8');
}

// Elements of `const NAME = ['a', 'b'];` in a script.
function arrayConstant(source: string, name: string): string[] {
  const match = source.match(new RegExp(`const ${name} = \\[([^\\]]*)\\]`));
  if (!match) {
    throw new Error(`${name} not found`);
  }
  return [...match[1]!.matchAll(/'([^']+)'/g)].map((item) => item[1]!);
}

// Items of a top-level YAML list, such as `noOverwrite:`.
function yamlList(source: string, key: string, indent = ''): string[] {
  const lines = source.split('\n');
  const start = lines.indexOf(`${indent}${key}:`);
  if (start < 0) {
    throw new Error(`${key} not found`);
  }
  const items: string[] = [];
  for (const line of lines.slice(start + 1)) {
    const item = line.match(new RegExp(`^${indent}  - (.+)$`));
    if (item) {
      items.push(item[1]!);
    } else if (!line.trimStart().startsWith('#')) {
      break;
    }
  }
  return items;
}

const ubrnConfig = read(packageDir, 'ubrn.config.yaml');
const buildScript = read(packageDir, 'scripts', 'build-native.mjs');

describe('Android TLS init', () => {
  test('test_paykit_android_binds_to_exported_jni_symbol', () => {
    const kotlin = read(
      packageDir,
      'android/src/main/java/com/synonym/paykit/PaykitAndroid.kt'
    );
    const pkg = kotlin.match(/^package ([\w.]+)$/m)?.[1];
    const object = kotlin.match(/^public object (\w+)/m)?.[1];
    const method = kotlin.match(/external fun (\w+)\(context: Context\)/)?.[1];
    const symbol = `Java_${pkg?.replace(/\./g, '_')}_${object}_${method}`;

    expect(symbol).toBe(
      'Java_com_synonym_paykit_PaykitAndroid_nativeInitialize'
    );
    expect(read(repoRoot, 'paykit-ffi/src/lib.rs')).toContain(
      `pub extern "system" fn ${symbol}(`
    );
    expect(kotlin).toContain('System.loadLibrary("paykit")');
    expect(read(packageDir, 'scripts/android-elf.mjs')).toContain(
      `'${symbol}'`
    );
  });

  test('test_turbo_module_initializes_tls_verifier', () => {
    const module = read(
      packageDir,
      'android/src/main/java/com/synonymdev/reactnativepaykit/ReactNativePaykitModule.kt'
    );
    expect(module).toMatch(
      /init \{[^}]*PaykitAndroid\.initializeOrThrow\(reactContext\)/
    );
  });

  test('test_android_build_packages_verifier_classes', () => {
    const gradle = read(packageDir, 'android/build.gradle');
    expect(gradle).toContain('file("libs/rustls-platform-verifier.jar")');
    expect(gradle).toContain('implementation files(rustlsPlatformVerifierJar)');
    expect(read(packageDir, 'android/proguard-rules.pro')).toContain(
      'class org.rustls.platformverifier.** { *; }'
    );
  });
});

describe('ubrn configuration', () => {
  test('test_cmake_links_paykit_shared_library', () => {
    const cmake = read(packageDir, 'android/CMakeLists.txt');
    expect(ubrnConfig).toContain('useSharedLibrary: true');
    expect(cmake).toContain('/jniLibs/${ANDROID_ABI}/libpaykit.so');
    expect(cmake).toContain('add_library(my_rust_lib SHARED IMPORTED)');
    expect(cmake).not.toMatch(/MY_RUST_LIB[^)]*libpaykit_ffi/);
  });

  test('test_hand_edited_glue_is_not_overwritten', () => {
    expect(yamlList(ubrnConfig, 'noOverwrite').sort()).toEqual(
      [
        'ReactNativePaykit.podspec',
        'android/CMakeLists.txt',
        'android/build.gradle',
        'android/src/main/java/com/synonymdev/reactnativepaykit/ReactNativePaykitModule.kt',
      ].sort()
    );
  });

  test('test_podspec_tag_matches_release_tags', () => {
    expect(read(packageDir, 'ReactNativePaykit.podspec')).toContain(
      ':tag => "v#{s.version}"'
    );
  });
});

describe('native build script', () => {
  test('test_build_targets_match_ubrn_config', () => {
    const targets = (section: string) =>
      yamlList(
        ubrnConfig.slice(ubrnConfig.indexOf(`\n${section}:`)),
        'targets',
        '  '
      );
    expect(arrayConstant(buildScript, 'IOS_TARGETS')).toEqual(targets('ios'));
    const androidElf = read(packageDir, 'scripts', 'android-elf.mjs');
    expect(arrayConstant(androidElf, 'ANDROID_ABIS')).toEqual(
      targets('android')
    );
  });

  test('test_ios_deployment_target_matches_build_ios_script', () => {
    const buildIos = read(repoRoot, 'paykit-ffi/build_ios.sh');
    const expected = buildIos.match(/IPHONEOS_DEPLOYMENT_TARGET=([\d.]+)/)?.[1];
    expect(expected).toBeDefined();
    expect(buildScript).toContain(`IOS_DEPLOYMENT_TARGET = '${expected}'`);
  });
});
