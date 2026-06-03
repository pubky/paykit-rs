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

export interface PrivatePaymentList {
  payment_endpoints: PaymentEndpoint[];
}

export interface PrivateApplicationMessage {
  version: number | null;
  kind: string | null;
  raw_json: string;
}

export interface PaymentAmount {
  value: string;
  asset: string;
}

export interface BillingPeriod {
  starts_at: string;
  ends_at: string;
}

export type RecurrenceUnit =
  | 'minute'
  | 'hour'
  | 'day'
  | 'week'
  | 'month'
  | 'year';

export interface Recurrence {
  every: number;
  unit: RecurrenceUnit;
  starts_at: string;
  anchor: string;
  ends_at: string | null;
}

export interface PaymentRequestTerms {
  amount: PaymentAmount;
  payment_reference: string;
  proposal_expires_at: string | null;
  recurrence: Recurrence | null;
  accepted_payment_endpoint_identifiers: string[];
  metadata?: Record<string, unknown>;
}

export interface PaymentRequest {
  event_id: string;
  payment_request_id: string;
  request: PaymentRequestTerms;
}

export interface PaymentRequestAcceptance {
  event_id: string;
  payment_request_id: string;
}

export interface PaymentRequestRejection {
  event_id: string;
  payment_request_id: string;
  reason?: string | null;
}

export interface PaymentRequestCancellation {
  event_id: string;
  payment_request_id: string;
  reason?: string | null;
}

export interface PaymentProof {
  event_id: string;
  payment_request_id: string;
  payment_reference: string;
  billing_period: BillingPeriod | null;
  payment_endpoint_identifier: string;
  proof: Record<string, unknown>;
}

export type PaymentRequestEvent =
  | {
      event_type: 'request';
      request: PaymentRequest;
      acceptance?: null;
      rejection?: null;
      cancellation?: null;
      proof?: null;
    }
  | {
      event_type: 'acceptance';
      request?: null;
      acceptance: PaymentRequestAcceptance;
      rejection?: null;
      cancellation?: null;
      proof?: null;
    }
  | {
      event_type: 'rejection';
      request?: null;
      acceptance?: null;
      rejection: PaymentRequestRejection;
      cancellation?: null;
      proof?: null;
    }
  | {
      event_type: 'cancellation';
      request?: null;
      acceptance?: null;
      rejection?: null;
      cancellation: PaymentRequestCancellation;
      proof?: null;
    }
  | {
      event_type: 'proof';
      request?: null;
      acceptance?: null;
      rejection?: null;
      cancellation?: null;
      proof: PaymentProof;
    };

export interface PaymentRequestEventMessage {
  kind: string;
  event_id: string | null;
  payment_request_id: string | null;
  raw_json: string;
  event: PaymentRequestEvent | null;
  validation_error: string | null;
}

export interface ReceiptDraft {
  receipt_id?: string | null;
  payment_reference: string;
  payment_request_id?: string | null;
  billing_period?: BillingPeriod | null;
  payment_endpoint_identifier?: string | null;
  amount?: PaymentAmount | null;
  metadata?: Record<string, unknown>;
}

export interface Receipt {
  receipt_id: string;
  payment_reference: string;
  payment_request_id: string | null;
  billing_period: BillingPeriod | null;
  recipient_public_key: string;
  payment_endpoint_identifier: string | null;
  amount: PaymentAmount | null;
  metadata: Record<string, unknown>;
}

export interface ReceiptAccess {
  event_id: string;
  receipt_id: string;
  payment_reference: string;
  payment_request_id: string | null;
  billing_period: BillingPeriod | null;
  location: string;
  key: string;
}

export interface ReceiptAccessEventMessage {
  kind: string;
  event_id: string | null;
  receipt_id: string | null;
  raw_json: string;
  access: ReceiptAccess | null;
  validation_error: string | null;
}

export interface PreparedReceipt {
  receipt: Receipt;
  encrypted_receipt: string;
  access: ReceiptAccess;
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
 * Returns null if not set.
 */
export async function getPaymentEndpoint(
  publicKey: string,
  paymentEndpointIdentifier: string
): Promise<Result<string | null>> {
  try {
    const res: Array<string | null> = await Paykit.getPaymentEndpoint(
      publicKey,
      paymentEndpointIdentifier
    );
    if (res[0] === 'error') {
      return err(res[1] ?? '');
    }
    return ok(res[1] ?? null);
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
    const res: string[] = await Paykit.removePaymentEndpoint(
      paymentEndpointIdentifier
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

// ---------------------------------------------------------------------------
// Private Payment Lists and Encrypted Links
// ---------------------------------------------------------------------------

/**
 * Default number of automatic send retries for Private Payment List updates.
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
 * Encrypt and send the complete Private Payment List.
 */
export async function setPrivatePaymentList(
  linkId: EncryptedLinkHandle,
  list: PrivatePaymentList
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.setPrivatePaymentList(
      linkId,
      JSON.stringify(list)
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
 * Receive all currently available Private Application Messages.
 */
export async function receivePrivateApplicationMessages(
  linkId: EncryptedLinkHandle
): Promise<Result<PrivateApplicationMessage[]>> {
  try {
    const res: string[] = await Paykit.receivePrivateApplicationMessages(
      linkId
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!) as PrivateApplicationMessage[]);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Parse a Private Payment List JSON message.
 */
export async function parsePrivatePaymentListJson(
  json: string
): Promise<Result<PrivatePaymentList>> {
  try {
    const res: string[] = await Paykit.parsePrivatePaymentListJson(json);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!) as PrivatePaymentList);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Parse a raw private message as a Payment Request event.
 */
export async function parsePaymentRequestEventMessage(
  message: PrivateApplicationMessage
): Promise<Result<PaymentRequestEventMessage | null>> {
  try {
    const res: string[] = await Paykit.parsePaymentRequestEventMessage(
      JSON.stringify(message)
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!) as PaymentRequestEventMessage | null);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Serialize a Payment Request event to canonical JSON.
 */
export async function serializePaymentRequestEvent(
  event: PaymentRequestEvent
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.serializePaymentRequestEvent(
      JSON.stringify(event)
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
 * Validate a Payment Proof against a Payment Request's immutable terms.
 */
export async function validatePaymentProofForRequest(
  proof: PaymentProof,
  request: PaymentRequest
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.validatePaymentProofForRequest(
      JSON.stringify(proof),
      JSON.stringify(request)
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
 * Send a Payment Request proposal event.
 */
export async function sendPaymentRequest(
  linkId: EncryptedLinkHandle,
  event: PaymentRequest
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.sendPaymentRequest(
      linkId,
      JSON.stringify(event)
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
 * Send a Payment Request acceptance event.
 */
export async function sendPaymentRequestAcceptance(
  linkId: EncryptedLinkHandle,
  event: PaymentRequestAcceptance
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.sendPaymentRequestAcceptance(
      linkId,
      JSON.stringify(event)
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
 * Send a Payment Request rejection event.
 */
export async function sendPaymentRequestRejection(
  linkId: EncryptedLinkHandle,
  event: PaymentRequestRejection
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.sendPaymentRequestRejection(
      linkId,
      JSON.stringify(event)
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
 * Send a Payment Request cancellation event.
 */
export async function sendPaymentRequestCancellation(
  linkId: EncryptedLinkHandle,
  event: PaymentRequestCancellation
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.sendPaymentRequestCancellation(
      linkId,
      JSON.stringify(event)
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
 * Send a Payment Proof event.
 */
export async function sendPaymentProof(
  linkId: EncryptedLinkHandle,
  event: PaymentProof
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.sendPaymentProof(
      linkId,
      JSON.stringify(event)
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
 * Prepare a plaintext Receipt, Encrypted Receipt, and matching Receipt Access descriptor.
 */
export async function prepareReceipt(
  linkId: EncryptedLinkHandle,
  draft: ReceiptDraft
): Promise<Result<PreparedReceipt>> {
  try {
    const res: string[] = await Paykit.prepareReceipt(
      linkId,
      JSON.stringify(draft)
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!) as PreparedReceipt);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Store a prepared Encrypted Receipt at its Receipt Location.
 */
export async function storePreparedReceipt(
  prepared: PreparedReceipt
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.storePreparedReceipt(
      JSON.stringify(prepared)
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
 * Send a prepared Receipt Access descriptor over an Encrypted Link.
 */
export async function sendReceiptAccess(
  linkId: EncryptedLinkHandle,
  access: ReceiptAccess
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.sendReceiptAccess(
      linkId,
      JSON.stringify(access)
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
 * Parse a raw private message as a Receipt Access event.
 */
export async function parseReceiptAccessEventMessage(
  message: PrivateApplicationMessage
): Promise<Result<ReceiptAccessEventMessage | null>> {
  try {
    const res: string[] = await Paykit.parseReceiptAccessEventMessage(
      JSON.stringify(message)
    );
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!) as ReceiptAccessEventMessage | null);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Parse a Receipt Access JSON message.
 */
export async function parseReceiptAccessJson(
  json: string
): Promise<Result<ReceiptAccess>> {
  try {
    const res: string[] = await Paykit.parseReceiptAccessJson(json);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(JSON.parse(res[1]!) as ReceiptAccess);
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Return the canonical homeserver Receipt Location path for a Receipt ID.
 */
export async function receiptLocation(
  receiptId: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.receiptLocation(receiptId);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}

/**
 * Decrypt an Encrypted Receipt fetched from the homeserver.
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
