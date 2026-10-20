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
});
