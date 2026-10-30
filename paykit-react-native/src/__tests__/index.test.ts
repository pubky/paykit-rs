import { readFileSync } from 'node:fs';
import path from 'node:path';

// The TurboModule and the JSI bindings need a native runtime. Jest has none,
// so stub the installer and the checksum check, and test the TS facade only.
jest.mock('../NativeReactNativePaykit', () => ({
  __esModule: true,
  default: {
    installRustCrate: jest.fn(() => true),
    cleanupRustCrate: jest.fn(() => true),
  },
}));
jest.mock('../generated/paykit', () => ({
  ...jest.requireActual('../generated/paykit'),
  __esModule: true,
  default: { initialize: jest.fn() },
}));

const packageDir = path.join(__dirname, '..', '..');

function redactedBindingTypes(): string[] {
  const file = path.join(
    packageDir,
    '..',
    'paykit-ffi',
    'redacted_binding_types.txt'
  );
  return readFileSync(file, 'utf8')
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith('#'));
}

// The backticked names between "Do not log" and "This list comes from" in the
// README's Security section.
function readmeRedactedTypes(): string[] {
  const readme = readFileSync(path.join(packageDir, 'README.md'), 'utf8');
  const security = readme.slice(readme.indexOf('\n## Security\n'));
  const list = security.slice(
    security.indexOf('Do not log'),
    security.indexOf('This list comes from')
  );
  return [...list.matchAll(/`(Ffi[A-Za-z]+)`/g)].map((match) => match[1]!);
}

describe('package entrypoint', () => {
  test('test_entrypoint_installs_and_initializes_bindings_once', () => {
    const native = require('../NativeReactNativePaykit').default;
    const bindings = require('../generated/paykit').default;
    require('../index');
    require('../index');
    expect(native.installRustCrate).toHaveBeenCalledTimes(1);
    expect(bindings.initialize).toHaveBeenCalledTimes(1);
  });

  test('test_entrypoint_exports_sdk_surface', () => {
    const paykit = require('../index');
    for (const name of [
      'FfiPaykitSdk',
      'FfiSdkStateBlob',
      'defaultConfig',
      'requiredSessionCapabilities',
      'uniffiInitAsync',
    ]) {
      expect(typeof paykit[name]).toBe('function');
    }
    expect(paykit.default.paykit).toBeDefined();
    // TypeScript keeps the Rust names; Swift and Kotlin strip the Ffi prefix.
    expect(paykit.PaykitSdk).toBeUndefined();
  });

  test('test_entrypoint_exports_typed_errors', () => {
    const { PaykitFfiError, PaykitFfiError_Tags } = require('../index');
    const error = new PaykitFfiError.ConcurrentUpdate({
      code: 'state_revision_conflict',
      context: 'test',
    });
    expect(error).toBeInstanceOf(Error);
    expect(PaykitFfiError.instanceOf(error)).toBe(true);
    expect(error.tag).toBe(PaykitFfiError_Tags.ConcurrentUpdate);
    expect(PaykitFfiError.instanceOf(new Error('plain'))).toBe(false);
  });
});

describe('README security section', () => {
  test('test_readme_redacted_types_match_ffi_list', () => {
    const expected = redactedBindingTypes().map((name) => `Ffi${name}`);
    expect(readmeRedactedTypes().sort()).toEqual(expected.sort());
  });

  test('test_redacted_binding_types_exist_in_generated_bindings', () => {
    const generated = readFileSync(
      path.join(packageDir, 'src', 'generated', 'paykit.ts'),
      'utf8'
    );
    const missing = redactedBindingTypes().filter(
      (name) => !generated.includes(`export type Ffi${name} = `)
    );
    expect(missing).toEqual([]);
  });
});
