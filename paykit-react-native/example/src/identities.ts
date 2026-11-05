// TEST-ONLY IDENTITIES. These mnemonics are public BIP39 test vectors from
// the Trezor reference suite (https://github.com/trezor/python-mnemonic,
// vectors.json). Anyone can derive their keys. Never use them, or keys
// derived from them, for real funds or real accounts, and never replace them
// with a real mnemonic.
//
// Fixed identities let a staging run sign in again with the same accounts
// after their single-use signup tokens are spent.

export type TestIdentity = {
  /** Label used in the UI and in smoke logs. Never log the mnemonic. */
  label: 'A' | 'B';
  mnemonic: string;
};

export const TEST_IDENTITY_A: TestIdentity = {
  label: 'A',
  mnemonic:
    'letter advice cage absurd amount doctor acoustic avoid letter advice cage above',
};

export const TEST_IDENTITY_B: TestIdentity = {
  label: 'B',
  mnemonic: 'zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong',
};

/** Paykit App ID both test identities publish under. */
export const EXAMPLE_APP_ID = 'paykit-rn-example';

/** Pubky client ID recorded in the identities' Pubky grants. */
export const EXAMPLE_CLIENT_ID = 'com.synonym.paykit.rnexample';
