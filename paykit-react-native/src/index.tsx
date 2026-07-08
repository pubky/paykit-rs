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

type NativeResult = [status: 'ok' | 'error', value?: string | null];

export interface PaykitErrorInfo {
  category:
    | 'storage'
    | 'identity'
    | 'transport'
    | 'not_found'
    | 'protocol'
    | 'policy'
    | 'payment_adapter'
    | 'recovery_required'
    | 'platform'
    | 'unknown';
  code: string;
  context: string;
}

export type EndpointManagementScope =
  | 'managed_only'
  | 'full_paykit_namespace'
  | 'unknown';

export type EncryptedLinkRecoveryMarkerPolicy =
  | 'enabled'
  | 'disabled'
  | 'unknown';

export type PublicContactSharingPolicy =
  | 'local_only'
  | 'configured_public_namespace'
  | 'unknown';

export type PubkyIdentityCapability =
  | 'signed_out'
  | 'public_only'
  | 'private_link_capable'
  | 'unknown';

export type PubkyAuthRequestKind =
  | 'sign_in'
  | 'sign_up'
  | 'secret_export'
  | 'unknown';

export interface PaykitSdkConfig {
  receiverPath: string;
  profileNamespace: string;
  endpointManagementScope: EndpointManagementScope;
  encryptedLinkRecoveryMarkers: EncryptedLinkRecoveryMarkerPolicy;
  publicContactSharing: PublicContactSharingPolicy;
  peerLinkOperationLeaseTimeoutSecs: number;
  outboundPrivateSendLeaseTimeoutSecs: number;
  outboundPrivateRetryBackoffSecs: number;
}

export interface PubkyClientConfig {
  requestTimeoutSecs: number;
}

export interface PubkyAuthDetails {
  kind: PubkyAuthRequestKind;
  capabilities: string | null;
  relayUrl: string | null;
  homeserverPublicKey: string | null;
}

export interface PubkyResourceRef {
  publicKey: string;
  path: string;
  transportUrl: string;
}

interface NativePaykitSdkConfig {
  receiver_path: string;
  profile_namespace: string;
  endpoint_management_scope: EndpointManagementScope;
  encrypted_link_recovery_markers: EncryptedLinkRecoveryMarkerPolicy;
  public_contact_sharing: PublicContactSharingPolicy;
  peer_link_operation_lease_timeout_secs: number;
  outbound_private_send_lease_timeout_secs: number;
  outbound_private_retry_backoff_secs: number;
}

interface NativePubkyClientConfig {
  request_timeout_secs: number;
}

interface NativePubkyAuthDetails {
  kind: PubkyAuthRequestKind;
  capabilities: string | null;
  relay_url: string | null;
  homeserver_public_key: string | null;
}

interface NativePubkyResourceRef {
  public_key: string;
  path: string;
  transport_url: string;
}

interface NativePaykit {
  sdkDefaultConfig(receiverPath: string): Promise<NativeResult>;
  sdkDefaultPubkyClientConfig(): Promise<NativeResult>;
  sdkRequiredSessionCapabilities(configJson: string): Promise<NativeResult>;
  sdkPubkyPublicKeyFromBip39Seed(seedBase64: string): Promise<NativeResult>;
  sdkPubkyPublicKeyFromBip39Mnemonic(
    mnemonicPhrase: string
  ): Promise<NativeResult>;
  sdkParsePubkyAuthUrl(authUrl: string): Promise<NativeResult>;
  sdkResolvePubkyUrl(uri: string): Promise<NativeResult>;
  sdkParsePubkyResource(uri: string): Promise<NativeResult>;
}

const Native = Paykit as NativePaykit;

function resultValue(res: NativeResult): Result<string> {
  if (res[0] === 'error') {
    return err(res[1] ?? unknownErrorPayload('missing native error'));
  }
  return ok(res[1] ?? '');
}

function resultJson<T>(res: NativeResult): Result<T> {
  if (res[0] === 'error') {
    return err(res[1] ?? unknownErrorPayload('missing native error'));
  }
  try {
    return ok(JSON.parse(res[1] ?? '') as T);
  } catch (e) {
    return err(unknownErrorPayload(e));
  }
}

function unknownErrorPayload(_error: unknown): string {
  return JSON.stringify({
    category: 'platform',
    code: 'platform_error',
    context: 'native module call failed',
  } satisfies PaykitErrorInfo);
}

export function parsePaykitError(error: string): PaykitErrorInfo {
  try {
    const parsed = JSON.parse(error) as Partial<PaykitErrorInfo>;
    if (
      typeof parsed.category === 'string' &&
      typeof parsed.code === 'string' &&
      typeof parsed.context === 'string'
    ) {
      return parsed as PaykitErrorInfo;
    }
  } catch {
    // Fall through to the generic error wrapper below.
  }
  return {
    category: 'unknown',
    code: 'unstructured_error',
    context: 'unstructured native error',
  };
}

function paykitSdkConfigFromNative(
  config: NativePaykitSdkConfig
): PaykitSdkConfig {
  return {
    receiverPath: config.receiver_path,
    profileNamespace: config.profile_namespace,
    endpointManagementScope: config.endpoint_management_scope,
    encryptedLinkRecoveryMarkers: config.encrypted_link_recovery_markers,
    publicContactSharing: config.public_contact_sharing,
    peerLinkOperationLeaseTimeoutSecs:
      config.peer_link_operation_lease_timeout_secs,
    outboundPrivateSendLeaseTimeoutSecs:
      config.outbound_private_send_lease_timeout_secs,
    outboundPrivateRetryBackoffSecs: config.outbound_private_retry_backoff_secs,
  };
}

function paykitSdkConfigToNative(
  config: PaykitSdkConfig
): NativePaykitSdkConfig {
  return {
    receiver_path: config.receiverPath,
    profile_namespace: config.profileNamespace,
    endpoint_management_scope: config.endpointManagementScope,
    encrypted_link_recovery_markers: config.encryptedLinkRecoveryMarkers,
    public_contact_sharing: config.publicContactSharing,
    peer_link_operation_lease_timeout_secs:
      config.peerLinkOperationLeaseTimeoutSecs,
    outbound_private_send_lease_timeout_secs:
      config.outboundPrivateSendLeaseTimeoutSecs,
    outbound_private_retry_backoff_secs: config.outboundPrivateRetryBackoffSecs,
  };
}

function pubkyClientConfigFromNative(
  config: NativePubkyClientConfig
): PubkyClientConfig {
  return {
    requestTimeoutSecs: config.request_timeout_secs,
  };
}

function pubkyAuthDetailsFromNative(
  details: NativePubkyAuthDetails
): PubkyAuthDetails {
  return {
    kind: details.kind,
    capabilities: details.capabilities,
    relayUrl: details.relay_url,
    homeserverPublicKey: details.homeserver_public_key,
  };
}

function pubkyResourceRefFromNative(
  resource: NativePubkyResourceRef
): PubkyResourceRef {
  return {
    publicKey: resource.public_key,
    path: resource.path,
    transportUrl: resource.transport_url,
  };
}

export async function defaultConfig(
  receiverPath: string
): Promise<Result<PaykitSdkConfig>> {
  try {
    const result = resultJson<NativePaykitSdkConfig>(
      await Native.sdkDefaultConfig(receiverPath)
    );
    return result.isErr()
      ? err(result.error)
      : ok(paykitSdkConfigFromNative(result.value));
  } catch (e) {
    return err(unknownErrorPayload(e));
  }
}

export async function defaultPubkyClientConfig(): Promise<
  Result<PubkyClientConfig>
> {
  try {
    const result = resultJson<NativePubkyClientConfig>(
      await Native.sdkDefaultPubkyClientConfig()
    );
    return result.isErr()
      ? err(result.error)
      : ok(pubkyClientConfigFromNative(result.value));
  } catch (e) {
    return err(unknownErrorPayload(e));
  }
}

export async function requiredSessionCapabilities(
  config: PaykitSdkConfig
): Promise<Result<string>> {
  try {
    return resultValue(
      await Native.sdkRequiredSessionCapabilities(
        JSON.stringify(paykitSdkConfigToNative(config))
      )
    );
  } catch (e) {
    return err(unknownErrorPayload(e));
  }
}

export async function pubkyPublicKeyFromBip39Seed(
  seedBase64: string
): Promise<Result<string>> {
  try {
    return resultValue(
      await Native.sdkPubkyPublicKeyFromBip39Seed(seedBase64)
    );
  } catch (e) {
    return err(unknownErrorPayload(e));
  }
}

export async function pubkyPublicKeyFromBip39Mnemonic(
  mnemonicPhrase: string
): Promise<Result<string>> {
  try {
    return resultValue(
      await Native.sdkPubkyPublicKeyFromBip39Mnemonic(mnemonicPhrase)
    );
  } catch (e) {
    return err(unknownErrorPayload(e));
  }
}

export async function parsePubkyAuthUrl(
  authUrl: string
): Promise<Result<PubkyAuthDetails>> {
  try {
    const result = resultJson<NativePubkyAuthDetails>(
      await Native.sdkParsePubkyAuthUrl(authUrl)
    );
    return result.isErr()
      ? err(result.error)
      : ok(pubkyAuthDetailsFromNative(result.value));
  } catch (e) {
    return err(unknownErrorPayload(e));
  }
}

export async function resolvePubkyUrl(uri: string): Promise<Result<string>> {
  try {
    return resultValue(await Native.sdkResolvePubkyUrl(uri));
  } catch (e) {
    return err(unknownErrorPayload(e));
  }
}

export async function parsePubkyResource(
  uri: string
): Promise<Result<PubkyResourceRef>> {
  try {
    const result = resultJson<NativePubkyResourceRef>(
      await Native.sdkParsePubkyResource(uri)
    );
    return result.isErr()
      ? err(result.error)
      : ok(pubkyResourceRefFromNative(result.value));
  } catch (e) {
    return err(unknownErrorPayload(e));
  }
}
