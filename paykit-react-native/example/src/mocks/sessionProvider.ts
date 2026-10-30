// A Pubky session provider backed by real homeserver sessions.
//
// `signUpOrSignIn` creates the session with the SDK's own Pubky bootstrap
// helper. The provider then hands the SDK the live session access that the
// bootstrap returned, so the SDK reuses that session instead of restoring the
// grant again. Session access holds the grant secret and the local secret
// key: a real app keeps it in platform secure storage. This one only keeps it
// in memory and never logs it.
import {
  FfiPubkySessionBootstrap,
  paykitAuthorizerSessionCapabilities,
  pubkySecretKeyFromBip39Mnemonic,
  type FfiPubkyClientConfig,
  type FfiPubkySessionAccessInterface,
  type FfiPubkySessionBootstrapResult,
  type FfiSdkPubkySessionProvider,
} from '@synonymdev/react-native-paykit';

import { EXAMPLE_CLIENT_ID, type TestIdentity } from '../identities';

export class LiveSessionProvider implements FfiSdkPubkySessionProvider {
  constructor(private access: FfiPubkySessionAccessInterface | undefined) {}

  loadSessionAccess(): FfiPubkySessionAccessInterface | undefined {
    return this.access;
  }

  publicStorageAvailable(): boolean {
    return true;
  }

  clearSessionAccess(): void {
    this.access = undefined;
  }
}

export type SignedInIdentity = {
  label: TestIdentity['label'];
  /** `pubky...` public key. Public, so it may be logged in redacted form. */
  publicKey: string;
  provider: LiveSessionProvider;
};

/**
 * Sign up when a signup token is given or the homeserver is the open local
 * testnet; otherwise sign in to the existing account.
 *
 * The grant asks for authorizer capabilities: the SDK needs the authority
 * path to publish the Paykit Noise Key Authorization before Encrypted Links.
 */
export async function signUpOrSignIn(
  identity: TestIdentity,
  options: {
    pubkyClient: FfiPubkyClientConfig;
    homeserver: string;
    signUp: boolean;
    signupToken: string | undefined;
  }
): Promise<SignedInIdentity> {
  const bootstrap = FfiPubkySessionBootstrap.withPubkyClientConfig(
    EXAMPLE_CLIENT_ID,
    options.pubkyClient
  );
  const secret = pubkySecretKeyFromBip39Mnemonic(identity.mnemonic);
  const capabilities = paykitAuthorizerSessionCapabilities();
  const result: FfiPubkySessionBootstrapResult = options.signUp
    ? await bootstrap.signUp(
        secret,
        options.homeserver,
        options.signupToken,
        capabilities
      )
    : await bootstrap.signIn(secret, capabilities);
  return {
    label: identity.label,
    publicKey: result.publicKey,
    provider: new LiveSessionProvider(result.sessionAccess),
  };
}
