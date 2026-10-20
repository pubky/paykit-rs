// The smoke steps. Each step logs one line,
//   SMOKE_STEP <name> pass|fail|skip <short reason>
// and the run ends with `SMOKE_RESULT pass|fail`, which CI greps for.
//
// Reasons are written here or are PaykitFfiError tags and codes. They never
// contain secrets, mnemonics, signup tokens, payloads or state-blob bytes.
import {
  FfiLinkedPeerState,
  FfiPaykitSdk,
  FfiPaymentReference,
  FfiPaymentRequestLifecycleState,
  FfiPrivateJsonObject,
  FfiPrivatePaymentResolutionStatus,
  FfiPubkyIdentityCapability,
  FfiPublicPaymentResolutionStatus,
  PaykitFfiError,
  PaykitFfiError_Tags,
  defaultConfig,
  defaultPubkyClientConfig,
  pubkyPublicKeyFromSecret,
  pubkySecretKeyFromBip39Mnemonic,
  requiredSessionCapabilities,
  type FfiPaykitSdkConfig,
  type FfiPaykitSdkInterface,
  type FfiPubkyClientConfig,
} from '@synonymdev/react-native-paykit';

import {
  EXAMPLE_APP_ID,
  TEST_IDENTITY_A,
  TEST_IDENTITY_B,
  type TestIdentity,
} from './identities';
import type { LaunchConfig } from './launchConfig';
import { MockWallet, mockReceivingDetails } from './mocks/mockWallet';
import {
  LiveSessionProvider,
  signUpOrSignIn,
  type SignedInIdentity,
} from './mocks/sessionProvider';
import { InMemoryStateBlobStore } from './mocks/stateBlobStore';

export type StepStatus = 'pending' | 'running' | 'pass' | 'fail' | 'skip';

const STEP_TIMEOUT_MS = 90_000;
const POLL_INTERVAL_MS = 250;
const LIGHTNING = 'btc-lightning-bolt11';

/** A step outcome other than pass. Thrown by a step to end it early. */
class StepSkipped extends Error {}

/** One participant: an identity with its SDK handle, store and wallet. */
type Party = SignedInIdentity & {
  sdk: FfiPaykitSdkInterface;
  store: InMemoryStateBlobStore;
  wallet: MockWallet;
};

type SmokeState = {
  config: LaunchConfig;
  platform: string;
  sdkConfig: FfiPaykitSdkConfig;
  pubkyClient: FfiPubkyClientConfig;
  a?: Party;
  b?: Party;
  paymentRequestId?: string;
};

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Reject after `ms`, so a deadlocked callback fails the step, not the run. */
function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(
      () => reject(new Error(`timed out after ${ms / 1000} s`)),
      ms
    );
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/** Poll until `done` accepts a result, or fail with `what`. */
async function pollUntil<T>(
  what: string,
  read: () => Promise<T>,
  done: (value: T) => boolean,
  timeoutMs = 30_000
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await read();
    if (done(value)) {
      return value;
    }
    if (Date.now() > deadline) {
      throw new Error(`${what} did not happen within ${timeoutMs / 1000} s`);
    }
    await sleep(POLL_INTERVAL_MS);
  }
}

function check(condition: boolean, failure: string): asserts condition {
  if (!condition) {
    throw new Error(failure);
  }
}

/** A short, secret-free description of a failure. */
export function describeError(error: unknown): string {
  if (PaykitFfiError.instanceOf(error)) {
    return `PaykitFfiError.${error.tag} ${error.inner.code}`;
  }
  if (error instanceof Error) {
    return error.message.slice(0, 160);
  }
  return 'unknown error';
}

function requireParties(state: SmokeState): { a: Party; b: Party } {
  if (!state.a || !state.b) {
    throw new StepSkipped('needs sign_up');
  }
  return { a: state.a, b: state.b };
}

function buildSdk(
  state: SmokeState,
  party: Pick<Party, 'store' | 'provider' | 'wallet'>
): FfiPaykitSdkInterface {
  return FfiPaykitSdk.withPaymentAdapterAndPubkyClientConfig(
    party.store,
    party.provider,
    party.wallet,
    state.sdkConfig,
    state.pubkyClient
  );
}

/** Poll until `payee` sees Payment Request `id` with `payer` in `state`. */
async function awaitRequestState(
  what: string,
  payee: Party,
  payer: Party,
  id: string,
  state: FfiPaymentRequestLifecycleState
): Promise<void> {
  await pollUntil(
    what,
    () => payee.sdk.paymentRequestsWith(payer.publicKey),
    (records) =>
      records.some(
        (record) => record.paymentRequestId === id && record.state === state
      )
  );
}

/** Send `sender`'s queued private messages and receive them at `receiver`. */
async function deliver(sender: Party, receiver: Party): Promise<void> {
  const sent = await sender.sdk.processOutboundPrivateMessages(
    receiver.publicKey
  );
  check(sent.failed.length === 0, `${sender.label} failed to send`);
  check(sent.sent.length > 0, `${sender.label} had nothing to send`);
  await pollUntil(
    `${receiver.label} receiving from ${sender.label}`,
    () => receiver.sdk.receivePrivateMessages(sender.publicKey),
    (report) => report.streamItemIds.length > 0
  );
}

async function stepOffline(): Promise<string> {
  const config = defaultConfig(EXAMPLE_APP_ID);
  check(config.appId === EXAMPLE_APP_ID, 'defaultConfig lost the App ID');
  check(
    defaultPubkyClientConfig().localTestnetHost === undefined,
    'default Pubky client config is not the public network'
  );
  check(
    requiredSessionCapabilities().includes('/pub/paykit/'),
    'required capabilities do not cover /pub/paykit/'
  );
  const keys = [TEST_IDENTITY_A, TEST_IDENTITY_B].map((identity) =>
    pubkyPublicKeyFromSecret(pubkySecretKeyFromBip39Mnemonic(identity.mnemonic))
  );
  check(
    keys.every((key) => key.startsWith('pubky')),
    'public key is not in pubky form'
  );
  check(keys[0] !== keys[1], 'A and B derived the same public key');
  return 'config, capabilities and both public keys derived';
}

async function stepSignUp(state: SmokeState): Promise<string> {
  const signUp = async (
    identity: TestIdentity,
    signupToken: string | undefined,
    store: InMemoryStateBlobStore
  ): Promise<Party> => {
    const signedIn = await signUpOrSignIn(identity, {
      pubkyClient: state.pubkyClient,
      homeserver: state.config.homeserver,
      // The local testnet homeserver accepts signups without a token.
      signUp: state.config.mode === 'local' || signupToken !== undefined,
      signupToken,
    });
    const wallet = new MockWallet(identity.label);
    const sdk = buildSdk(state, { ...signedIn, store, wallet });
    const status = await sdk.initialize();
    check(
      status.capability === FfiPubkyIdentityCapability.PrivateLinkCapable,
      `${identity.label} cannot establish Encrypted Links`
    );
    await sdk.publishPaykitNoiseKeyAuthorization();
    await sdk.publishPaykitApp('Paykit React Native example', {
      privatePayments: true,
      paymentRequests: true,
      receipts: true,
      outgoingPayments: true,
    });
    return { ...signedIn, sdk, store, wallet };
  };
  // A's store exports and reloads the blob bytes, so `restart` can rebuild A
  // from them.
  state.a = await signUp(
    TEST_IDENTITY_A,
    state.config.signupTokenA,
    new InMemoryStateBlobStore('persisted')
  );
  state.b = await signUp(
    TEST_IDENTITY_B,
    state.config.signupTokenB,
    new InMemoryStateBlobStore('memory')
  );
  return `${state.config.mode} homeserver, both initialized`;
}

async function stepPublicEndpoint(state: SmokeState): Promise<string> {
  const { a, b } = requireParties(state);
  const published = await a.sdk.syncPublicEndpoints();
  check(published.failed.length === 0, 'A failed to publish');
  check(published.published.length > 0, 'A published nothing');

  const resolved = await b.sdk.resolvePublicContactPayment(
    a.publicKey,
    undefined
  );
  check(
    resolved.status === FfiPublicPaymentResolutionStatus.Payable,
    'B cannot pay A publicly'
  );
  const expected = mockReceivingDetails(a.label).find(
    (detail) => detail.identifier === LIGHTNING
  )!.payload;
  check(
    resolved.payableEndpoints.some(
      (endpoint) =>
        endpoint.identifier === LIGHTNING &&
        endpoint.target.payload.exportText() === expected
    ),
    `B did not resolve A's ${LIGHTNING} endpoint`
  );

  const removed = await a.sdk.syncPublicEndpointsWithReceivingDetails([]);
  check(removed.removed.length > 0, 'A removed nothing');
  await pollUntil(
    'B seeing the endpoint gone',
    () => b.sdk.resolvePublicContactPayment(a.publicKey, undefined),
    (resolution) =>
      resolution.status === FfiPublicPaymentResolutionStatus.NoEndpoint
  );
  return `${published.published.length} published, resolved, removed`;
}

async function stepPrivatePaymentList(state: SmokeState): Promise<string> {
  const { a, b } = requireParties(state);
  await a.sdk.initiateLinkWithPeer(b.publicKey);
  await b.sdk.acceptLinkWithPeer(a.publicKey);
  let aState = FfiLinkedPeerState.Linking;
  let bState = FfiLinkedPeerState.Linking;
  await pollUntil(
    'the Encrypted Link Handshake',
    async () => {
      if (aState !== FfiLinkedPeerState.Linked) {
        aState = (await a.sdk.advanceLinkHandshake(b.publicKey)).state;
      }
      if (bState !== FfiLinkedPeerState.Linked) {
        bState = (await b.sdk.advanceLinkHandshake(a.publicKey)).state;
      }
      return [aState, bState];
    },
    (states) => states.every((s) => s === FfiLinkedPeerState.Linked)
  );

  await a.sdk.enqueuePrivatePaymentList(b.publicKey);
  await deliver(a, b);
  const lists = await b.sdk.currentPrivatePaymentLists(a.publicKey);
  const list = lists.find((view) => view.appId === EXAMPLE_APP_ID);
  check(list !== undefined, "B has no Private Payment List from A's App");
  check(
    list.paymentEndpoints.some((endpoint) => endpoint.identifier === LIGHTNING),
    `A's Private Payment List has no ${LIGHTNING} endpoint`
  );
  return `linked, B received ${list.paymentEndpoints.length} endpoints`;
}

async function stepPaymentRequest(state: SmokeState): Promise<string> {
  const { a, b } = requireParties(state);
  // A, the payee, asks B to pay.
  const proposed = await a.sdk.proposePaymentRequest(b.publicKey, {
    amount: { value: '0.00001', asset: 'btc' },
    paymentReference: new FfiPaymentReference('paykit-rn-example-smoke'),
    proposalExpiresAt: undefined,
    recurrence: undefined,
    acceptedPaymentEndpointIdentifiers: [LIGHTNING],
    paymentEndpoints: undefined,
    requiredAppId: undefined,
    conversion: undefined,
    paymentDeadline: undefined,
    metadata: new FfiPrivateJsonObject('{}'),
  });
  const id = proposed.paymentRequestId;
  state.paymentRequestId = id;
  await deliver(a, b);

  const received = (await b.sdk.receivedPaymentRequestsFrom(a.publicKey)).find(
    (record) => record.paymentRequestId === id
  );
  check(
    received?.proposalStreamItemId !== undefined,
    'B did not receive the Payment Request'
  );
  await b.sdk.claimAndAcceptPaymentRequest(
    a.publicKey,
    id,
    received.proposalStreamItemId
  );
  await deliver(b, a);
  await awaitRequestState(
    'A seeing the acceptance',
    a,
    b,
    id,
    FfiPaymentRequestLifecycleState.Accepted
  );

  // B resolves where to pay from A's Private Payment List, "pays" with the
  // mock wallet (no funds move), and submits a Payment Proof.
  const resolution = await b.sdk.resolvePrivatePaymentRequest(
    a.publicKey,
    id,
    undefined
  );
  check(
    resolution.status === FfiPrivatePaymentResolutionStatus.Payable,
    'B cannot pay the Payment Request privately'
  );
  await b.sdk.submitPaymentProof(a.publicKey, id, {
    billingPeriod: undefined,
    paymentAppId: EXAMPLE_APP_ID,
    paymentEndpointIdentifier: LIGHTNING,
    allowanceId: undefined,
    conversionQuoteId: undefined,
    proof: new FfiPrivateJsonObject('{"test_evidence":"paykit-rn-example"}'),
  });
  await deliver(b, a);
  await awaitRequestState(
    'A receiving the Payment Proof',
    a,
    b,
    id,
    FfiPaymentRequestLifecycleState.ProofSubmitted
  );
  return 'proposed, accepted, resolved, proof received';
}

async function stepRestart(state: SmokeState): Promise<string> {
  const { a, b } = requireParties(state);
  check(a.store.revision !== undefined, 'A has no stored state blob');
  const loadsBefore = a.store.loads;
  // A new handle on the same store: its async calls load the blob through
  // the synchronous store callback, from a Rust worker thread.
  const restarted = buildSdk(state, a);
  const status = await restarted.initialize();
  check(status.publicKey === a.publicKey, 'restarted A has another identity');
  const peers = await restarted.linkedPeers();
  check(
    peers.some(
      (peer) =>
        peer.counterparty === b.publicKey &&
        peer.state === FfiLinkedPeerState.Linked
    ),
    'restarted A lost the Encrypted Link'
  );
  if (state.paymentRequestId) {
    const requests = await restarted.paymentRequestsWith(b.publicKey);
    check(
      requests.some(
        (record) => record.paymentRequestId === state.paymentRequestId
      ),
      'restarted A lost the Payment Request'
    );
  }
  check(a.store.loads > loadsBefore, 'the store callback was never called');
  state.a = { ...a, sdk: restarted };
  return `reloaded from ${a.store.loads - loadsBefore} store loads`;
}

async function stepTypedError(state: SmokeState): Promise<string> {
  // An offline handle: the identifier is validated before any I/O.
  const sdk = new FfiPaykitSdk(
    new InMemoryStateBlobStore(),
    new LiveSessionProvider(undefined),
    state.sdkConfig
  );
  try {
    await sdk.setDefaultPaykitAppForEndpoint('../private', undefined);
  } catch (error) {
    check(PaykitFfiError.instanceOf(error), 'error is not a PaykitFfiError');
    check(
      error.tag === PaykitFfiError_Tags.Protocol &&
        error.inner.code === 'validation',
      `got ${describeError(error)}`
    );
    return 'PaykitFfiError.Protocol validation';
  }
  throw new Error('the invalid identifier was accepted');
}

/**
 * The local stack serves plain HTTP, and Pubky's own TLS checks homeservers
 * against their public keys. Only the public pkarr relays use the platform
 * certificate verifier, so this step needs `staging` mode.
 */
async function stepAndroidHttps(state: SmokeState): Promise<string> {
  if (state.platform !== 'android') {
    throw new StepSkipped('Android only');
  }
  if (state.config.mode !== 'staging') {
    throw new StepSkipped('needs staging mode');
  }
  const { a } = requireParties(state);
  // A fresh handle has no cached record, so it resolves A over the network:
  // the public pkarr relays, over HTTPS.
  const fresh = buildSdk(state, {
    ...a,
    store: new InMemoryStateBlobStore(),
  });
  const registry = await fresh.paykitAppRegistry(a.publicKey);
  check(registry !== undefined, "A's Paykit App Registry did not resolve");
  return 'resolved through the public pkarr relays';
}

type Step = {
  name: string;
  title: string;
  run: (state: SmokeState) => Promise<string>;
  /** Defaults to STEP_TIMEOUT_MS. */
  timeoutMs?: number;
};

/** The steps, in the order they run. */
export const STEPS = [
  {
    name: 'offline',
    title: 'Configuration and public keys, offline',
    run: stepOffline,
  },
  {
    name: 'sign_up',
    title: 'Sign up A and B on the homeserver',
    run: stepSignUp,
  },
  {
    name: 'public_endpoint',
    title: 'Publish and remove a Payment Endpoint',
    run: stepPublicEndpoint,
  },
  {
    name: 'private_payment_list',
    title: 'Send a Private Payment List',
    run: stepPrivatePaymentList,
  },
  {
    name: 'payment_request',
    title: 'Payment Request round trip',
    run: stepPaymentRequest,
  },
  {
    name: 'restart',
    title: 'Restart A from the stored state blob',
    run: stepRestart,
    timeoutMs: 30_000,
  },
  {
    name: 'typed_error',
    title: 'Invalid Payment Endpoint Identifier',
    run: stepTypedError,
  },
  {
    name: 'android_https',
    title: 'HTTPS through Paykit on Android',
    run: stepAndroidHttps,
  },
] as const satisfies ReadonlyArray<Step>;

export type StepName = (typeof STEPS)[number]['name'];

export type StepResult = {
  name: StepName;
  title: string;
  status: StepStatus;
  reason: string;
};

function logStep(result: StepResult): void {
  console.log(`SMOKE_STEP ${result.name} ${result.status} ${result.reason}`);
}

/**
 * Run every step in order. A failed step does not stop the run; steps that
 * depend on it skip. The run passes when no step failed.
 */
export async function runSmoke(
  config: LaunchConfig,
  platform: string,
  onUpdate: (result: StepResult) => void
): Promise<boolean> {
  const pubkyClient: FfiPubkyClientConfig = {
    ...defaultPubkyClientConfig(),
    localTestnetHost: config.mode === 'local' ? config.localHost : undefined,
  };
  const state: SmokeState = {
    config,
    platform,
    sdkConfig: defaultConfig(EXAMPLE_APP_ID),
    pubkyClient,
  };
  let passed = true;
  for (const step of STEPS) {
    const { name, title } = step;
    onUpdate({ name, title, status: 'running', reason: '' });
    const timeout = 'timeoutMs' in step ? step.timeoutMs : STEP_TIMEOUT_MS;
    let result: StepResult;
    try {
      const reason = await withTimeout(step.run(state), timeout);
      result = { name, title, status: 'pass', reason };
    } catch (error) {
      const skipped = error instanceof StepSkipped;
      passed &&= skipped;
      result = {
        name,
        title,
        status: skipped ? 'skip' : 'fail',
        reason: describeError(error),
      };
    }
    logStep(result);
    onUpdate(result);
  }
  console.log(`SMOKE_RESULT ${passed ? 'pass' : 'fail'}`);
  return passed;
}
