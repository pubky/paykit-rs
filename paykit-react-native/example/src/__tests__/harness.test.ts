// Runs in the package's own Jest (`npm test` in paykit-react-native), without
// installing the example. The package is replaced by its generated error and
// enum types, which are plain TypeScript, plus stand-ins for the three native
// objects the mocks construct.
jest.mock(
  '@synonymdev/react-native-paykit',
  () => {
    const generated = jest.requireActual('../../../src/generated/paykit');
    class NativeStandIn {
      private readonly inner: unknown;
      constructor(inner: unknown) {
        this.inner = inner;
      }
      exportBytes() {
        return this.inner;
      }
      exportText() {
        return this.inner;
      }
    }
    return {
      PaykitFfiError: generated.PaykitFfiError,
      PaykitFfiError_Tags: generated.PaykitFfiError_Tags,
      FfiPrivateReceivingDetailReservationResponseKind:
        generated.FfiPrivateReceivingDetailReservationResponseKind,
      FfiSdkStateBlob: class FfiSdkStateBlob extends NativeStandIn {},
      FfiPaymentPayload: class FfiPaymentPayload extends NativeStandIn {},
      FfiReservationAttribution: class FfiReservationAttribution extends NativeStandIn {},
    };
  },
  { virtual: true }
);

const {
  PaykitFfiError,
  PaykitFfiError_Tags,
  FfiPrivateReceivingDetailReservationResponseKind,
  FfiSdkStateBlob,
} = require('@synonymdev/react-native-paykit');
const { InMemoryStateBlobStore } = require('../mocks/stateBlobStore');
const { MockWallet } = require('../mocks/mockWallet');
const {
  parseLaunchConfig,
  LOCAL_HOMESERVER,
  STAGING_HOMESERVER,
} = require('../launchConfig');

function blob(bytes: number[]) {
  return new FfiSdkStateBlob(new Uint8Array(bytes).buffer);
}

function expectConcurrentUpdate(save: () => unknown) {
  let thrown: unknown;
  try {
    save();
  } catch (error) {
    thrown = error;
  }
  expect(PaykitFfiError.instanceOf(thrown)).toBe(true);
  expect((thrown as { tag: string }).tag).toBe(
    PaykitFfiError_Tags.ConcurrentUpdate
  );
}

describe('example state blob store', () => {
  test('test_state_store_saves_with_matching_revision', () => {
    const store = new InMemoryStateBlobStore();
    expect(store.loadStateBlob()).toBeUndefined();
    const first = store.saveStateBlobAtomically(blob([1]), undefined);
    const second = store.saveStateBlobAtomically(blob([2]), first);
    expect(second).not.toBe(first);
    expect(store.loadStateBlob().revision).toBe(second);
  });

  test('test_state_store_rejects_stale_revision_without_writing', () => {
    const store = new InMemoryStateBlobStore();
    const first = store.saveStateBlobAtomically(blob([1]), undefined);
    store.saveStateBlobAtomically(blob([2]), first);
    const current = store.loadStateBlob();

    expectConcurrentUpdate(() =>
      store.saveStateBlobAtomically(blob([3]), first)
    );
    // A blob is stored, so a save that expects none is stale too.
    expectConcurrentUpdate(() =>
      store.saveStateBlobAtomically(blob([3]), undefined)
    );
    expect(store.loadStateBlob()).toEqual(current);
  });

  test('test_state_store_revisions_are_unique_across_stores', () => {
    const revisions = new Set<string>();
    for (let i = 0; i < 20; i++) {
      const store = new InMemoryStateBlobStore();
      let revision: string | undefined;
      for (let j = 0; j < 5; j++) {
        revision = store.saveStateBlobAtomically(blob([j]), revision);
        revisions.add(revision!);
      }
    }
    expect(revisions.size).toBe(100);
  });

  test('test_state_store_persisted_mode_reloads_copied_bytes', () => {
    const store = new InMemoryStateBlobStore('persisted');
    const saved = blob([7, 8, 9]);
    store.saveStateBlobAtomically(saved, undefined);
    // Changing the saved blob's buffer afterwards must not change the store.
    new Uint8Array(saved.exportBytes()).fill(0);

    const loaded = store.loadStateBlob();
    expect(loaded.blob).not.toBe(saved);
    expect([...new Uint8Array(loaded.blob.exportBytes())]).toEqual([7, 8, 9]);
    expect(store.loads).toBe(1);
  });
});

describe('example mock wallet', () => {
  const counterparty = 'pubkyexample';

  test('test_mock_wallet_reservations_are_deterministic', () => {
    const ids = () =>
      new MockWallet('A')
        .reservePrivateReceivingDetails(counterparty)
        .reservations.map((r: { reservationId: string }) => r.reservationId);
    expect(ids()).toEqual(ids());
    expect(ids()).toEqual(['mock-reservation-A-1', 'mock-reservation-A-2']);
    expect(
      new MockWallet('A').reservePrivateReceivingDetails(counterparty).kind
    ).toBe(FfiPrivateReceivingDetailReservationResponseKind.Reservations);
  });

  test('test_mock_wallet_cancel_is_idempotent', () => {
    const wallet = new MockWallet('A');
    const [first] =
      wallet.reservePrivateReceivingDetails(counterparty).reservations;
    const cancel = (reservationId: string) =>
      wallet.cancelPrivateReceivingDetailReservation({
        reservationId,
        counterparty,
        identifier: 'btc-lightning-bolt11',
        payloadHash: 'unused',
        attribution: first.attribution,
      });
    cancel(first.reservationId);
    cancel(first.reservationId);
    cancel('mock-reservation-unknown');
    expect(wallet.cancelledReservations).toEqual([first.reservationId]);
  });

  test('test_mock_wallet_selects_supported_endpoints_in_preference_order', () => {
    const candidate = (candidateId: string, identifier: string) => ({
      candidateId,
      identifier,
    });
    const ids = new MockWallet('B').selectPublicPaymentEndpointIds({
      counterparty,
      amount: undefined,
      candidates: [
        candidate('onchain', 'btc-bitcoin-p2tr'),
        candidate('sepa', 'eur-sepa-iban'),
        candidate('lightning', 'btc-lightning-bolt11'),
      ],
    });
    expect(ids).toEqual(['lightning', 'onchain']);
  });

  test('test_mock_wallet_details_are_fake_test_strings', () => {
    for (const detail of new MockWallet('A').currentPublicReceivingDetails()) {
      expect(detail.payload.exportText()).toContain('paykit-rn-example-fake');
    }
  });
});

describe('example launch config', () => {
  test('test_launch_config_defaults_to_staging', () => {
    const config = parseLaunchConfig({}, 'ios');
    expect(config).toMatchObject({
      mode: 'staging',
      autorun: false,
      homeserver: STAGING_HOMESERVER,
      signupTokenA: undefined,
    });
  });

  test('test_launch_config_reads_local_mode_per_platform', () => {
    const props = { paykitMode: 'local', paykitAutorun: 'true' };
    expect(parseLaunchConfig(props, 'ios')).toMatchObject({
      mode: 'local',
      autorun: true,
      homeserver: LOCAL_HOMESERVER,
      localHost: '127.0.0.1',
    });
    expect(parseLaunchConfig(props, 'android').localHost).toBe('10.0.2.2');
  });

  test('test_launch_config_ignores_unknown_mode_and_blank_tokens', () => {
    const config = parseLaunchConfig(
      { paykitMode: 'mainnet', paykitSignupTokenA: '  ' },
      'android'
    );
    expect(config.mode).toBe('staging');
    expect(config.signupTokenA).toBeUndefined();
  });
});

describe('example smoke steps', () => {
  const { STEPS } = require('../smoke');

  test('test_smoke_steps_keep_names_and_order', () => {
    // CI and the README refer to the steps by these names.
    expect(STEPS.map((step: { name: string }) => step.name)).toEqual([
      'offline',
      'sign_up',
      'public_endpoint',
      'private_payment_list',
      'payment_request',
      'restart',
      'typed_error',
      'android_https',
    ]);
  });

  test('test_smoke_steps_only_restart_has_a_shorter_timeout', () => {
    const custom = STEPS.filter(
      (step: { timeoutMs?: number }) => step.timeoutMs !== undefined
    ).map((step: { name: string; timeoutMs: number }) => [
      step.name,
      step.timeoutMs,
    ]);
    expect(custom).toEqual([['restart', 30_000]]);
  });
});
