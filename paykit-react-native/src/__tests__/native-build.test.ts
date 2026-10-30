import { readFileSync } from 'node:fs';
import path from 'node:path';

// Static checks on the hand-edited native glue and build settings that no
// build checks: they only show up on a device.

const packageDir = path.join(__dirname, '..', '..');
const repoRoot = path.join(packageDir, '..');

function read(...segments: string[]): string {
  return readFileSync(path.join(...segments), 'utf8');
}

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

  test('test_proguard_keeps_verifier_classes', () => {
    expect(read(packageDir, 'android/proguard-rules.pro')).toContain(
      'class org.rustls.platformverifier.** { *; }'
    );
  });
});

describe('Android TurboModule install', () => {
  test('test_cpp_adapter_does_not_read_removed_hybrid_data_field', () => {
    // React Native 0.80 removed CallInvokerHolderImpl.mHybridData, so the
    // ubrn 0.29.3-1 adapter aborts at launch. ubrn.config.yaml keeps the
    // fixed adapter out of regeneration.
    const adapter = read(packageDir, 'android/cpp-adapter.cpp');
    expect(adapter).not.toContain('mHybridData');
    expect(adapter).toContain('holderRef->cthis()');
    expect(read(packageDir, 'ubrn.config.yaml')).toMatch(
      /noOverwrite:[\s\S]*- android\/cpp-adapter\.cpp/
    );
  });
});

describe('iOS build', () => {
  test('test_ios_deployment_target_matches_build_ios_script', () => {
    const buildIos = read(repoRoot, 'paykit-ffi/build_ios.sh');
    const expected = buildIos.match(/IPHONEOS_DEPLOYMENT_TARGET=([\d.]+)/)?.[1];
    const { scripts } = JSON.parse(read(packageDir, 'package.json'));
    expect(expected).toBeDefined();
    expect(scripts['build:ios:xcframework']).toContain(
      `IPHONEOS_DEPLOYMENT_TARGET=${expected} `
    );
  });

  test('test_ios_simulator_builds_are_arm64_only', () => {
    // The XCFramework has no x86_64 simulator slice, so simulator builds
    // that also build x86_64 fail to link.
    expect(read(packageDir, 'ubrn.config.yaml')).not.toContain(
      'x86_64-apple-ios'
    );
    expect(read(packageDir, 'example/ios/Podfile')).toMatch(
      /'EXCLUDED_ARCHS\[sdk=iphonesimulator\*\]'[\s\S]*= 'x86_64'/
    );
    const workflow = read(repoRoot, '.github/workflows/react-native.yml');
    const simulatorBuilds = workflow
      .split(/\n\s*\n/)
      .filter((block) => /xcodebuild[\s\S]*-sdk iphonesimulator/.test(block));
    expect(simulatorBuilds).not.toHaveLength(0);
    for (const block of simulatorBuilds) {
      expect(block).toContain('ARCHS=arm64');
    }
  });

  test('test_ios_smoke_streams_info_level_logs', () => {
    // React Native logs console.log at the os_log Info level, which
    // `log stream` leaves out by default.
    expect(read(packageDir, 'example/scripts/ios-smoke.sh')).toMatch(
      /log stream --level info /
    );
  });
});

describe('example tarball install', () => {
  test('test_example_lock_has_no_tarball_integrity', () => {
    // With an integrity, npm installs the cached tarball for that hash instead
    // of the freshly packed one, so the example would run stale natives.
    const lock = JSON.parse(read(packageDir, 'example/package-lock.json'));
    const entry = lock.packages['node_modules/@synonymdev/react-native-paykit'];
    expect(entry.resolved).toBe(
      'file:tarballs/synonymdev-react-native-paykit.tgz'
    );
    expect(entry).not.toHaveProperty('integrity');
  });
});
