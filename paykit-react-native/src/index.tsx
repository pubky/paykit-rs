import { NativeModules, Platform } from 'react-native';
import { ok, err, type Result } from '@synonymdev/result';

const LINKING_ERROR =
  `The package '@pubky/react-native-paykit' doesn't seem to be linked. Make sure: \n\n` +
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

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface PaymentEntry {
  method_id: string;
  endpoint_data: string;
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/**
 * Initialize the PayKit SDK. Call once at app startup.
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
// Payment list (read)
// ---------------------------------------------------------------------------

/**
 * Fetch all published payment methods for a user.
 */
export async function getPaymentList(
  publicKey: string
): Promise<Result<PaymentEntry[]>> {
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
 * Fetch a single payment endpoint for a user and method.
 * Returns empty string if not set.
 */
export async function getPaymentEndpoint(
  publicKey: string,
  methodId: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.getPaymentEndpoint(publicKey, methodId);
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
  methodId: string,
  endpointData: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.setPaymentEndpoint(
      methodId,
      endpointData
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
  methodId: string
): Promise<Result<string>> {
  try {
    const res: string[] = await Paykit.removePaymentEndpoint(methodId);
    if (res[0] === 'error') {
      return err(res[1]!);
    }
    return ok(res[1] ?? '');
  } catch (e) {
    return err(JSON.stringify(e));
  }
}
