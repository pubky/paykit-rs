// Launch configuration for the smoke harness.
//
// iOS passes launch arguments (`-paykitMode local`) through NSUserDefaults and
// Android passes intent string extras (`--es paykitMode local`). The native
// entry points (AppDelegate.swift, MainActivity.kt) copy the keys below into
// the root component's initial props, so both platforms arrive here the same
// way.

export type NetworkMode = 'staging' | 'local';

export type LaunchConfig = {
  mode: NetworkMode;
  /** Run every smoke step at launch, without taps. */
  autorun: boolean;
  /** Homeserver public key the identities sign up on. */
  homeserver: string;
  /** Host running the local Pubky stack, used in `local` mode only. */
  localHost: string;
  /**
   * Single-use staging signup tokens. Never commit them, and never log them.
   * Without a token, the identity signs in to an existing account instead.
   */
  signupTokenA: string | undefined;
  signupTokenB: string | undefined;
};

/** Initial props the native entry points may set. All values are strings. */
export const LAUNCH_KEYS = [
  'paykitMode',
  'paykitAutorun',
  'paykitHomeserver',
  'paykitLocalHost',
  'paykitSignupTokenA',
  'paykitSignupTokenB',
] as const;

export type LaunchProps = Partial<Record<(typeof LAUNCH_KEYS)[number], string>>;

/** The staging homeserver behind staging.pubky.app. */
export const STAGING_HOMESERVER =
  'ufibwbmed6jeq9k4p583go95wofakh9fwpp4k734trq79pd9u1uy';

/**
 * The fixed homeserver key of the local `pubky-testnet` stack
 * (pubky-testnet's StaticTestnet; see example/scripts/local-stack.mjs).
 */
export const LOCAL_HOMESERVER =
  '8pinxxgqs41n4aididenw5apqp1urfmzdztr8jt4abrkdn435ewo';

/**
 * Where the app reaches the host's local stack. The iOS simulator shares the
 * host's loopback. The Android emulator reaches the host as 10.0.2.2; the
 * homeserver itself publishes a 127.0.0.1 address, so also run
 * `adb reverse tcp:6286 tcp:6286` and `adb reverse tcp:6287 tcp:6287`.
 */
export function defaultLocalHost(platform: string): string {
  return platform === 'android' ? '10.0.2.2' : '127.0.0.1';
}

export function defaultHomeserver(mode: NetworkMode): string {
  return mode === 'local' ? LOCAL_HOMESERVER : STAGING_HOMESERVER;
}

function nonEmpty(value: string | undefined): string | undefined {
  const trimmed = value?.trim();
  return trimmed ? trimmed : undefined;
}

export function parseLaunchConfig(
  props: LaunchProps,
  platform: string
): LaunchConfig {
  const mode: NetworkMode = props.paykitMode === 'local' ? 'local' : 'staging';
  return {
    mode,
    autorun: props.paykitAutorun === 'true' || props.paykitAutorun === '1',
    homeserver: nonEmpty(props.paykitHomeserver) ?? defaultHomeserver(mode),
    localHost: nonEmpty(props.paykitLocalHost) ?? defaultLocalHost(platform),
    signupTokenA: nonEmpty(props.paykitSignupTokenA),
    signupTokenB: nonEmpty(props.paykitSignupTokenB),
  };
}
