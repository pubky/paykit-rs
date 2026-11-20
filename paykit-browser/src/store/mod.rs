//! The host's durable store in a browser: [`OpfsStore`], the Switch client's `StateStore` on
//! the Origin Private File System, for a dedicated worker.
//!
//! The store's format and logic (`format.rs`, `core.rs`) do not call the browser: they work on
//! two slot files through a small crate-private file interface (`file.rs`), whose browser side
//! is in `opfs.rs`, next to `OpfsStore` itself. That keeps them testable with torn writes and
//! failing calls, natively and in browser engines.

/// A unit test that runs under libtest natively and under wasm-bindgen-test on wasm32.
#[cfg(test)]
macro_rules! store_test {
    ($(#[$meta:meta])* fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
        #[cfg_attr(not(target_arch = "wasm32"), test)]
        fn $name() $body
    };
}

mod core;
mod file;
mod format;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod opfs;

use std::fmt;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub use self::opfs::OpfsStore;

/// The longest store name, in bytes.
const MAX_NAME_LEN: usize = 64;

/// Why [`OpfsStore::open`] failed. No state was written in any case, though the store's
/// directory and empty files may have been created before the failure.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenError {
    /// The name is not 1 to 64 ASCII letters, digits, `-` or `_`.
    InvalidName,
    /// This code does not run where the browser gives synchronous file access to the Origin
    /// Private File System: a dedicated worker in a secure context, in an engine that has
    /// `FileSystemSyncAccessHandle`. A page, a shared worker and a service worker get this
    /// error, checked before any file system call.
    NotDedicatedWorker,
    /// Another worker or tab of this origin has the store open: the browser refused the
    /// exclusive handle on one of its files. Open it again once that one has dropped its store.
    InUse,
    /// The store's files hold no state this build can trust: both are damaged or torn, one is
    /// damaged, or torn in a way a first commit cannot leave, next to an empty one, or one
    /// passes its integrity check but holds a state no writer produces. The store is not
    /// repaired by discarding either file; recover it from a backup or remove it. The store is
    /// the directory of its name in the origin's OPFS root: once no worker holds it open,
    /// `removeEntry(name, { recursive: true })` on that root removes it.
    Corrupt,
    /// One of the store's files was written in a newer format than this build reads, so this
    /// build must not write over it. A file of this build's format whose format version alone
    /// changed on disk is a damaged file instead, not a newer format.
    NewerFormat {
        /// The format version found.
        version: u32,
    },
    /// The browser refused a file system call, for example because the origin has no storage
    /// it may use in this context.
    Unavailable {
        /// The name of the browser's error, such as `SecurityError` or `UnknownError`, or, for
        /// a read that failed in this crate, a short description such as `short read`.
        name: String,
    },
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::InvalidName => {
                f.write_str("the store name must be 1 to 64 ASCII letters, digits, '-' or '_'")
            }
            OpenError::NotDedicatedWorker => f.write_str(
                "the store needs synchronous file access, which only a dedicated worker in a \
                 secure context has",
            ),
            OpenError::InUse => f.write_str("another worker or tab has the store open"),
            OpenError::Corrupt => {
                f.write_str("the store's files hold no state that passes its checks")
            }
            OpenError::NewerFormat { version } => write!(
                f,
                "the store was written in format version {version}, newer than this build reads"
            ),
            OpenError::Unavailable { name } => {
                write!(f, "the browser refused to open the store: {name}")
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// Check a store name: 1 to 64 ASCII letters, digits, `-` and `_`. Without `.`, the names `.`
/// and `..` cannot be formed.
fn check_name(name: &str) -> Result<(), OpenError> {
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_';
    if (1..=MAX_NAME_LEN).contains(&name.len()) && name.bytes().all(allowed) {
        Ok(())
    } else {
        Err(OpenError::InvalidName)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    store_test! {
        fn test_store_name_is_checked() {
            for name in ["a", "wallet-1", "Store_2", &"x".repeat(MAX_NAME_LEN)] {
                assert_eq!(check_name(name), Ok(()), "{name}");
            }
            for name in ["", ".", "..", "a.b", "a/b", "a b", "\u{e9}", &"x".repeat(MAX_NAME_LEN + 1)] {
                assert_eq!(check_name(name), Err(OpenError::InvalidName), "{name}");
            }
        }
    }
}
