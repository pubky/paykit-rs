import { NativeModules, Platform } from 'react-native';
import { ok, err, type Result } from '@synonymdev/result';

const LINKING_ERROR =
  `The package '@synonymdev/react-native-paykit' doesn't seem to be linked. Make sure: \n\n` +
  Platform.select({ ios: "- You have run 'pod install'\n", default: '' }) +
  '- You rebuilt the app after installing the package\n' +
  '- You are not using Expo Go\n';

const Paykit = NativeModules.Paykit
  ? NativeModules.Paykit
  : new Proxy(
      {},
      {
        get() {
          throw new Error(LINKING_ERROR);
        },
      }
    );

const MAX_U32 = 0xffffffff;

function validateUint32(value: number, label: string): string | null {
  if (!Number.isInteger(value) || value < 0 || value > MAX_U32) {
    return `${label} must be an integer between 0 and ${MAX_U32}`;
  }
  return null;
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface PaymentEndpoint {
  payment_endpoint_identifier: string;
  payment_endpoint_payload: string;
}

export interface PrivatePaymentEnvelope {
  reference: string;
  payment_endpoints: PaymentEndpoint[];
}

export interface ReceiptMetadataField {
  key: string;
  value: string;
}

export interface ReceiptDraft {
  reference: string;
  payment_endpoint_identifier?: string | null;
  amount?: string | null;
  currency?: string | null;
  metadata?: ReceiptMetadataField[];
}

export interface Receipt {
  reference: string;
  recipient_public_key: string;
  payment_endpoint_identifier: string | null;
  amount: string | null;
  currency: string | null;
  metadata: ReceiptMetadataField[];
}

export interface ReceiptAccess {
  version: number;
  reference: string;
  location: string;
  key: string;
  algorithm: string;
}

export interface IssuedReceipt {
  reference: string;
  location: string;
  key: string;
}

declare const encryptedLinkHandleBrand: unique symbol;
declare const encryptedLinkHandshakeHandleBrand: unique symbol;

export type EncryptedLinkHandle = string & {
  readonly [encryptedLinkHandleBrand]: true;
};
export type EncryptedLinkHandshakeHandle = string & {
  readonly [encryptedLinkHandshakeHandleBrand]: true;
};

interface NativeHandshakeProgress {
  status: 'pending' | 'complete';
  handle_id: string;
}

export type HandshakeProgress =
  | {
      status: 'pending';
      handshakeHandle: EncryptedLinkHandshakeHandle;
    }
  | {
      status: 'complete';
      linkHandle: EncryptedLinkHandle;
    };

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/**
 * Initialize the React Native binding. Call once at app startup.
 */
export async function initialize(): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.initialize();
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

// ---------------------------------------------------------------------------
// Session queries
// ---------------------------------------------------------------------------

/**
 * Returns true if an authenticated session is currently active.
 */
export async function isAuthenticated(): Promise<Result<boolean>> {
  try {
    const res: string[] = await Paykit.isAuthenticated();
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] === 'true');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Returns the public key of the currently authenticated user, or empty string if none.
 */
export async function getCurrentPublicKey(): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.getCurrentPublicKey();
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Export the current session secret for persistence across app restarts.
 */
export async function exportSession(): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.exportSession();
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/**
 * Import a session from a compact session secret string.
 * Returns the public key on success.
 */
export async function importSession(
  sessionSecret: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.importSession(sessionSecret);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Sign up with a raw secret key (dev-auth only).
 * Returns the public key on success.
 */
export async function signUp(
  secretKeyHex: string,
  homeserverPublicKey: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.signUp(
      secretKeyHex,
      homeserverPublicKey
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Sign in with a raw secret key (dev-auth only).
 * Returns the public key on success.
 */
export async function signIn(secretKeyHex: string): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.signIn(secretKeyHex);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * End the current session on the homeserver and clear local state.
 */
export async function signOut(): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.signOut();
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Discard the local session without contacting the homeserver.
 */
export async function forceSignOut(): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.forceSignOut();
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

// ---------------------------------------------------------------------------
// Payment List (read)
// ---------------------------------------------------------------------------

/**
 * Fetch all published Payment Endpoints for a user.
 */
export async function getPaymentList(
  publicKey: string
): Promise<Result<PaymentEndpoint[]>> {
  try {
    const res: string[] = await Paykit.getPaymentList(publicKey);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!));
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Fetch a single Payment Endpoint Payload for a payee.
 * Returns empty string if not set.
 */
export async function getPaymentEndpoint(
  publicKey: string,
  paymentEndpointIdentifier: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.getPaymentEndpoint(publicKey, paymentEndpointIdentifier);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

// ---------------------------------------------------------------------------
// Payment endpoints (write)
// ---------------------------------------------------------------------------

/**
 * Publish or update a payment endpoint for the authenticated user.
 */
export async function setPaymentEndpoint(
  paymentEndpointIdentifier: string,
  paymentEndpointPayload: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.setPaymentEndpoint(
      paymentEndpointIdentifier,
      paymentEndpointPayload
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Remove a payment endpoint for the authenticated user.
 */
export async function removePaymentEndpoint(
  paymentEndpointIdentifier: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.removePaymentEndpoint(paymentEndpointIdentifier);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

// ---------------------------------------------------------------------------
// Private Payment Envelopes and Encrypted Links
// ---------------------------------------------------------------------------

/**
 * Default number of automatic send retries for Private Payment Envelope updates.
 */
export async function defaultMaxSendRetries(): Promise<Result<number>> {
  try {
    const res: string[] = await Paykit.defaultMaxSendRetries();
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(Number(res[1] ?? '0'));
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Default number of consecutive handshake recovery attempts.
 */
export async function defaultMaxRecoveryAttempts(): Promise<Result<number>> {
  try {
    const res: string[] = await Paykit.defaultMaxRecoveryAttempts();
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(Number(res[1] ?? '0'));
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Generate a fresh UUID-v4 Payment Reference.
 */
export async function generatePaymentReference(): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.generatePaymentReference();
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Start a private Encrypted Link handshake as the initiator.
 */
export async function initiateEncryptedLink(
  secretKeyHex: string,
  receiverPublicKey: string
): Promise<Result<EncryptedLinkHandshakeHandle>> {
  try {
    const res: string[] = await Paykit.initiateEncryptedLink(
      secretKeyHex,
      receiverPublicKey
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok((res[1] ?? '') as EncryptedLinkHandshakeHandle);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Start a private Encrypted Link handshake as the responder.
 */
export async function acceptEncryptedLink(
  secretKeyHex: string,
  senderPublicKey: string
): Promise<Result<EncryptedLinkHandshakeHandle>> {
  try {
    const res: string[] = await Paykit.acceptEncryptedLink(
      secretKeyHex,
      senderPublicKey
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok((res[1] ?? '') as EncryptedLinkHandshakeHandle);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Advance a private Encrypted Link handshake by one polling-safe step.
 */
export async function advanceHandshake(
  handshakeId: EncryptedLinkHandshakeHandle
): Promise<Result<HandshakeProgress>> {
  try {
    const res: string[] = await Paykit.advanceHandshake(handshakeId);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    const progress = JSON.parse(res[1]!) as NativeHandshakeProgress;
    if (progress.status === 'pending') {
      return ok({
        status: 'pending',
        handshakeHandle: progress.handle_id as EncryptedLinkHandshakeHandle,
      });
    }
    return ok({
      status: 'complete',
      linkHandle: progress.handle_id as EncryptedLinkHandle,
    });
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Configure automatic recovery attempts for a pending handshake.
 */
export async function setEncryptedLinkHandshakeMaxRecoveryAttempts(
  handshakeId: EncryptedLinkHandshakeHandle,
  max: number
): Promise<Result<string>> {
  try {
    const validationError = validateUint32(max, 'max recovery attempts');
    if (validationError !== null) {
      return err(validationError);
    }

    const res: string[] =
      await Paykit.setEncryptedLinkHandshakeMaxRecoveryAttempts(
        handshakeId,
        max
      );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Configure automatic send retries for an established Encrypted Link.
 */
export async function setEncryptedLinkMaxSendRetries(
  linkId: EncryptedLinkHandle,
  max: number
): Promise<Result<string>> {
  try {
    const validationError = validateUint32(max, 'max send retries');
    if (validationError !== null) {
      return err(validationError);
    }

    const res: string[] = await Paykit.setEncryptedLinkMaxSendRetries(
      linkId,
      max
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Encrypt and send the complete Private Payment Envelope.
 */
export async function setPrivatePaymentEnvelope(
  linkId: EncryptedLinkHandle,
  envelope: PrivatePaymentEnvelope
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.setPrivatePaymentEnvelope(
      linkId,
      JSON.stringify(envelope)
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Receive and decrypt the latest Private Payment Envelope.
 */
export async function getPrivatePaymentEnvelope(
  linkId: EncryptedLinkHandle
): Promise<Result<PrivatePaymentEnvelope | null>> {
  try {
    const res: string[] = await Paykit.getPrivatePaymentEnvelope(linkId);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1] ?? 'null'));
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Store an encrypted Receipt and send Receipt Access to the counterparty.
 */
export async function issueReceipt(
  linkId: EncryptedLinkHandle,
  draft: ReceiptDraft
): Promise<Result<IssuedReceipt>> {
  try {
    const res: string[] = await Paykit.issueReceipt(
      linkId,
      JSON.stringify(draft)
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!) as IssuedReceipt);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Receive all currently available Receipt Access descriptors.
 */
export async function getReceiptAccess(
  linkId: EncryptedLinkHandle
): Promise<Result<ReceiptAccess[]>> {
  try {
    const res: string[] = await Paykit.getReceiptAccess(linkId);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!) as ReceiptAccess[]);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Return the canonical homeserver Receipt Location for a Payment Reference.
 */
export async function receiptLocation(
  reference: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.receiptLocation(reference);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Decrypt an encrypted Receipt fetched from the homeserver.
 */
export async function decryptReceipt(
  encryptedJson: string,
  key: string,
  location: string
): Promise<Result<Receipt>> {
  try {
    const res: string[] = await Paykit.decryptReceipt(
      encryptedJson,
      key,
      location
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!) as Receipt);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Serialize a pending Encrypted Link Handshake snapshot as hex.
 */
export async function serializeEncryptedLinkHandshake(
  handshakeId: EncryptedLinkHandshakeHandle
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.serializeEncryptedLinkHandshake(
      handshakeId
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Serialize an established Encrypted Link snapshot as hex.
 */
export async function serializeEncryptedLink(
  linkId: EncryptedLinkHandle
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.serializeEncryptedLink(linkId);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Return the counterparty embedded in an Encrypted Link snapshot.
 */
export async function encryptedLinkSnapshotRecipient(
  snapshotHex: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.encryptedLinkSnapshotRecipient(
      snapshotHex
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Return the counterparty embedded in an Encrypted Link Handshake snapshot.
 */
export async function encryptedLinkHandshakeSnapshotRecipient(
  snapshotHex: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.encryptedLinkHandshakeSnapshotRecipient(
      snapshotHex
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Restore an established Encrypted Link from a hex snapshot.
 */
export async function restoreEncryptedLink(
  secretKeyHex: string,
  snapshotHex: string
): Promise<Result<EncryptedLinkHandle>> {
  try {
    const res: string[] = await Paykit.restoreEncryptedLink(
      secretKeyHex,
      snapshotHex
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok((res[1] ?? '') as EncryptedLinkHandle);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Restore a pending Encrypted Link handshake from a hex snapshot.
 */
export async function restoreEncryptedLinkHandshake(
  secretKeyHex: string,
  snapshotHex: string
): Promise<Result<EncryptedLinkHandshakeHandle>> {
  try {
    const res: string[] = await Paykit.restoreEncryptedLinkHandshake(
      secretKeyHex,
      snapshotHex
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok((res[1] ?? '') as EncryptedLinkHandshakeHandle);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Close an established Encrypted Link and release its native handle.
 */
export async function closeEncryptedLink(
  linkId: EncryptedLinkHandle
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.closeEncryptedLink(linkId);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Drop a pending handshake native handle.
 */
export async function dropEncryptedLinkHandshake(
  handshakeId: EncryptedLinkHandshakeHandle
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.dropEncryptedLinkHandshake(handshakeId);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}
