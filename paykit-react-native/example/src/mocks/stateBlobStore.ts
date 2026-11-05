// A synchronous in-memory SDK state store for the smoke harness.
//
// It follows the FfiSdkStateBlobStore contract (paykit-ffi/src/storage.rs):
// - a save whose expected revision does not match the stored one fails with
//   PaykitFfiError.ConcurrentUpdate and writes nothing, including a save that
//   expects no blob while one is stored;
// - every successful save returns a new revision that never named an earlier
//   blob.
//
// TEST ONLY: the blob holds SDK secrets. A real app must keep it in
// synchronous *encrypted* storage (see the package README, "Security"). This
// store never logs or inspects the blob.
import {
  FfiSdkStateBlob,
  PaykitFfiError,
  type FfiSdkStateBlobInterface,
  type FfiSdkStateBlobSnapshot,
  type FfiSdkStateBlobStore,
} from '@synonymdev/react-native-paykit';

export type StateBlobStoreMode =
  /** Keep the SDK's own blob object, so the bytes stay in Rust memory. */
  | 'memory'
  /**
   * Export the bytes on save and rebuild a blob on load, as an app that
   * writes the blob to disk would. A new SDK handle on the same store then
   * restarts from the stored bytes.
   */
  | 'persisted';

type StoredBlob = {
  revision: string;
  blob: FfiSdkStateBlobInterface | undefined;
  bytes: ArrayBuffer | undefined;
};

export class InMemoryStateBlobStore implements FfiSdkStateBlobStore {
  // Revisions are `<store id>-<counter>`. The store id makes them unique
  // across store instances, the counter within one. They are identifiers,
  // not secrets, so Math.random is enough.
  private readonly storeId = `${Date.now().toString(36)}${Math.random()
    .toString(36)
    .slice(2, 10)}`;
  private counter = 0;
  private stored: StoredBlob | undefined;
  /** Calls to `loadStateBlob` so far. */
  loads = 0;

  constructor(readonly mode: StateBlobStoreMode = 'memory') {}

  /** The current revision, without touching the blob. */
  get revision(): string | undefined {
    return this.stored?.revision;
  }

  loadStateBlob(): FfiSdkStateBlobSnapshot | undefined {
    this.loads += 1;
    const stored = this.stored;
    if (!stored) {
      return undefined;
    }
    const blob = stored.blob ?? new FfiSdkStateBlob(stored.bytes!.slice(0));
    return { blob, revision: stored.revision };
  }

  saveStateBlobAtomically(
    blob: FfiSdkStateBlobInterface,
    expectedRevision: string | undefined
  ): string {
    if (this.stored?.revision !== expectedRevision) {
      throw new PaykitFfiError.ConcurrentUpdate({
        code: 'state_revision_mismatch',
        context: 'stored SDK state revision changed',
      });
    }
    const revision = `${this.storeId}-${++this.counter}`;
    this.stored =
      this.mode === 'persisted'
        ? { revision, blob: undefined, bytes: blob.exportBytes().slice(0) }
        : { revision, blob, bytes: undefined };
    return revision;
  }
}
