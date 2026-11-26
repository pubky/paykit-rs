//! The file operations the store's core needs: [`SlotFile`], with the browser's file in
//! `opfs.rs` and an in-memory file for the unit tests.
//!
//! This is not a general storage abstraction: it has exactly the operations the two-slot
//! format uses, and exactly two implementations. It exists so the core's logic can be tested
//! with torn writes and failing calls, which a real browser file cannot be made to produce.

/// A failed file operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FileError {
    /// The origin's storage quota would be exceeded (`QuotaExceededError`).
    Quota,
    /// Any other failure: the browser error's name (such as `InvalidStateError`), or a
    /// description of a short read or write.
    Failed(String),
}

impl FileError {
    /// The name the store reports for this failure when it cannot open.
    pub(crate) fn name(&self) -> &str {
        match self {
            FileError::Quota => "QuotaExceededError",
            FileError::Failed(name) => name,
        }
    }
}

/// One slot file of the store, held open until the store and its leases have dropped.
///
/// Every call is synchronous. Offsets are always 0: a slot is written whole from the start of
/// its file, and read whole from there.
pub(crate) trait SlotFile {
    /// The file's size in bytes.
    fn size(&mut self) -> Result<u64, FileError>;

    /// The file's first `len` bytes. The caller has checked `len` against [`Self::size`] and
    /// the state cap; reading fewer bytes is an error.
    fn read_all(&mut self, len: usize) -> Result<Vec<u8>, FileError>;

    /// Write all of `bytes` from offset 0, leaving any bytes past them as they were. Writing
    /// fewer bytes is an error, and some of them may have been written.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FileError>;

    /// Cut the file to `len` bytes.
    fn truncate(&mut self, len: u64) -> Result<(), FileError>;

    /// Ask the browser to move every written byte to the storage device.
    fn flush(&mut self) -> Result<(), FileError>;
}

#[cfg(test)]
pub(crate) use memory::{MemoryDisk, MemoryFile, Op, OpKind};

/// The in-memory file for the unit tests: both slot files on one shared disk that the test
/// keeps, a log of every operation in order, and one-shot faults.
#[cfg(test)]
mod memory {
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use super::{FileError, SlotFile};

    /// One operation on a slot file, by slot index.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum Op {
        Size(usize),
        Read(usize, usize),
        Write(usize, usize),
        Truncate(usize, u64),
        Flush(usize),
        /// The file was dropped, which closes the browser's file.
        Close(usize),
    }

    /// An operation a fault can target.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum OpKind {
        Size,
        Write,
        Truncate,
        Flush,
    }

    #[derive(Default)]
    struct Disk {
        files: [Vec<u8>; 2],
        /// A size `size` reports for a slot instead of its real length.
        claimed_size: [Option<u64>; 2],
        ops: Vec<Op>,
        /// The next write stops after this many bytes, or after all of them if there are fewer,
        /// and fails, as a crash or a browser error would leave it.
        tear_at: Option<usize>,
        /// The next operation of this kind fails with this error, writing nothing.
        fail: Option<(OpKind, FileError)>,
    }

    /// The two slot files' bytes, shared by the files a store holds and the test.
    #[derive(Clone, Default)]
    pub(crate) struct MemoryDisk(Arc<Mutex<Disk>>);

    impl MemoryDisk {
        fn lock(&self) -> MutexGuard<'_, Disk> {
            // A failed assertion elsewhere must not turn a file's drop into a second panic.
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// A pair of files on this disk, as a store opens them.
        pub(crate) fn files(&self) -> [MemoryFile; 2] {
            [0, 1].map(|slot| MemoryFile {
                disk: self.clone(),
                slot,
            })
        }

        pub(crate) fn bytes(&self, slot: usize) -> Vec<u8> {
            self.lock().files[slot].clone()
        }

        pub(crate) fn set_bytes(&self, slot: usize, bytes: Vec<u8>) {
            self.lock().files[slot] = bytes;
        }

        pub(crate) fn claim_size(&self, slot: usize, size: u64) {
            self.lock().claimed_size[slot] = Some(size);
        }

        /// The operations since the last call, in order.
        pub(crate) fn take_ops(&self) -> Vec<Op> {
            std::mem::take(&mut self.lock().ops)
        }

        pub(crate) fn tear_next_write_at(&self, len: usize) {
            self.lock().tear_at = Some(len);
        }

        pub(crate) fn fail_next(&self, kind: OpKind, error: FileError) {
            self.lock().fail = Some((kind, error));
        }
    }

    /// One slot file on a [`MemoryDisk`].
    pub(crate) struct MemoryFile {
        disk: MemoryDisk,
        slot: usize,
    }

    impl MemoryFile {
        /// Log `op`, and fail with the pending fault if it targets `kind`.
        fn start(&self, op: Op, kind: Option<OpKind>) -> Result<MutexGuard<'_, Disk>, FileError> {
            let mut disk = self.disk.lock();
            disk.ops.push(op);
            match disk.fail.take() {
                Some((target, error)) if Some(target) == kind => Err(error),
                pending => {
                    disk.fail = pending;
                    Ok(disk)
                }
            }
        }
    }

    impl SlotFile for MemoryFile {
        fn size(&mut self) -> Result<u64, FileError> {
            let disk = self.start(Op::Size(self.slot), Some(OpKind::Size))?;
            Ok(disk.claimed_size[self.slot]
                .unwrap_or_else(|| u64::try_from(disk.files[self.slot].len()).unwrap()))
        }

        fn read_all(&mut self, len: usize) -> Result<Vec<u8>, FileError> {
            let disk = self.start(Op::Read(self.slot, len), None)?;
            disk.files[self.slot]
                .get(..len)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| FileError::Failed("short read".into()))
        }

        fn write_all(&mut self, bytes: &[u8]) -> Result<(), FileError> {
            let mut disk = self.start(Op::Write(self.slot, bytes.len()), Some(OpKind::Write))?;
            let (written, result) = match disk.tear_at.take() {
                Some(len) => (
                    &bytes[..len.min(bytes.len())],
                    Err(FileError::Failed("torn write".into())),
                ),
                None => (bytes, Ok(())),
            };
            let file = &mut disk.files[self.slot];
            if file.len() < written.len() {
                file.resize(written.len(), 0);
            }
            file[..written.len()].copy_from_slice(written);
            result
        }

        fn truncate(&mut self, len: u64) -> Result<(), FileError> {
            let mut disk = self.start(Op::Truncate(self.slot, len), Some(OpKind::Truncate))?;
            disk.files[self.slot].resize(usize::try_from(len).unwrap(), 0);
            Ok(())
        }

        fn flush(&mut self) -> Result<(), FileError> {
            self.start(Op::Flush(self.slot), Some(OpKind::Flush))
                .map(drop)
        }
    }

    impl Drop for MemoryFile {
        fn drop(&mut self) {
            drop(self.start(Op::Close(self.slot), None));
        }
    }
}
