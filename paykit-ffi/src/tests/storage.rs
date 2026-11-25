use crate::{StorageError, StoreEntry, StoreWrite, WalletStore};
use pubky_dispatch_core::enc::{b64, hash};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    sync::Mutex,
};

/// A connection to a test database file. The file is in WAL mode, set when it is created, so a
/// reader never blocks a commit; a writer waits up to 30 seconds for another, so a loaded
/// machine slows a test's write rather than failing it.
pub(super) fn connect(path: &Path) -> Connection {
    let db = Connection::open(path).unwrap();
    db.busy_timeout(std::time::Duration::from_secs(30)).unwrap();
    db
}

/// Creates the test database at `path` in WAL mode with `schema`.
pub(super) fn create(path: &Path, schema: &str) {
    let db = connect(path);
    let mode: String = db
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    db.execute_batch(schema).unwrap();
}

/// Device files opened independently by each host instance.
pub(super) struct DeviceStore {
    root: PathBuf,
    leases: Mutex<BTreeMap<String, (Vec<u8>, File)>>,
}
impl DeviceStore {
    pub(super) fn open(root: &Path) -> Self {
        let store = Self {
            root: root.to_owned(),
            leases: Mutex::new(BTreeMap::new()),
        };
        create(
            &root.join("wallet.sqlite"),
            "PRAGMA secure_delete=ON; CREATE TABLE IF NOT EXISTS wallet(ns TEXT,k BLOB,v BLOB,PRIMARY KEY(ns,k));",
        );
        store
    }
    pub(super) fn hold_enrollment_completion(root: &Path, hold: bool) {
        let db = connect(&root.join("wallet.sqlite"));
        if hold {
            db.execute_batch(
                "CREATE TRIGGER hold_completion BEFORE INSERT ON wallet
                WHEN new.ns LIKE 'paykit-enrollment/%'
                AND json_type(CAST(new.v AS TEXT), '$.Joined') = 'object'
                BEGIN SELECT RAISE(ABORT, 'enrollment completion held'); END;",
            )
            .unwrap();
        } else {
            db.execute_batch("DROP TRIGGER hold_completion").unwrap();
        }
    }
    /// Interrupt the native app's write after the enrollment has pinned its approval.
    pub(super) fn hold_installation(root: &Path, hold: bool) {
        let db = connect(&root.join("wallet.sqlite"));
        let statement = if hold {
            "CREATE TRIGGER hold_app BEFORE INSERT ON wallet
             WHEN new.ns NOT LIKE 'paykit-enrollment/%'
             BEGIN SELECT RAISE(ABORT, 'app installation held'); END;"
        } else {
            "DROP TRIGGER hold_app"
        };
        db.execute_batch(statement).unwrap();
    }

    fn db(&self) -> Connection {
        connect(&self.root.join("wallet.sqlite"))
    }
    fn reserve(&self, ns: &str) -> Result<Option<File>, StorageError> {
        let path = self.root.join(b64(&hash(ns.as_bytes())));
        let f = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| StorageError::Unavailable)?;
        match f.try_lock() {
            Ok(()) => Ok(Some(f)),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(_) => Err(StorageError::Unavailable),
        }
    }
    fn apply(&self, namespace: &str, writes: Vec<StoreWrite>) -> Result<(), StorageError> {
        let mut db = self.db();
        let tx = db.transaction().map_err(|_| StorageError::Unavailable)?;
        for w in writes {
            match w {
                StoreWrite::Put{key,value} => tx.execute("INSERT OR REPLACE INTO wallet VALUES(?1,?2,?3)",params![namespace,key,value]),
                StoreWrite::Delete{key} => tx.execute("DELETE FROM wallet WHERE ns=?1 AND k=?2",params![namespace,key]),
                StoreWrite::DeletePrefix{prefix} => tx.execute("DELETE FROM wallet WHERE ns=?1 AND (length(?2)=0 OR substr(k,1,length(?2))=?2)",params![namespace,prefix]),
            }.map_err(|_|StorageError::Unavailable)?;
        }
        tx.commit().map_err(|_| StorageError::Unknown)
    }
}
impl WalletStore for DeviceStore {
    fn get(&self, namespace: String, key: Vec<u8>) -> Result<Option<Vec<u8>>, StorageError> {
        self.db()
            .query_row(
                "SELECT v FROM wallet WHERE ns=?1 AND k=?2",
                params![namespace, key],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| StorageError::Unavailable)
    }
    fn scan(
        &self,
        namespace: String,
        prefix: Vec<u8>,
        after: Option<Vec<u8>>,
        limit: u32,
    ) -> Result<Vec<StoreEntry>, StorageError> {
        let db = self.db();
        let mut q=db.prepare("SELECT k,v FROM wallet WHERE ns=?1 AND (?2 IS NULL OR k>?2) AND (length(?3)=0 OR substr(k,1,length(?3))=?3) ORDER BY k LIMIT ?4").unwrap();
        let rows = q
            .query_map(params![namespace, after, prefix, limit], |r| {
                Ok(StoreEntry {
                    key: r.get(0)?,
                    value: r.get(1)?,
                })
            })
            .map_err(|_| StorageError::Unavailable)?;
        rows.collect::<Result<_, _>>()
            .map_err(|_| StorageError::Unavailable)
    }
    fn acquire(&self, namespace: String, owner: Vec<u8>) -> Result<bool, StorageError> {
        let Some(file) = self.reserve(&namespace)? else {
            return Ok(false);
        };
        self.leases.lock().unwrap().insert(namespace, (owner, file));
        if self.root.join("acquired-without-answer").exists() {
            return Err(StorageError::Unknown);
        }
        Ok(true)
    }
    fn release(&self, namespace: String, owner: Vec<u8>) -> Result<(), StorageError> {
        if self.root.join("hold-release").exists() {
            return Err(StorageError::Unavailable);
        }
        let mut locks = self.leases.lock().unwrap();
        if let Some((id, file)) = locks.get(&namespace) {
            if id == &owner {
                file.unlock().map_err(|_| StorageError::Unavailable)?;
                locks.remove(&namespace);
            }
        }
        Ok(())
    }
    fn commit_if(
        &self,
        namespace: String,
        owner: Vec<u8>,
        writes: Vec<StoreWrite>,
    ) -> Result<bool, StorageError> {
        let leases = self.leases.lock().unwrap();
        if !leases.get(&namespace).is_some_and(|(id, _)| id == &owner) {
            return Ok(false);
        }
        self.apply(&namespace, writes)?;
        Ok(true)
    }
}

/// A reader with a transaction open on the store's file doesn't make a commit fail: the commit
/// goes through while the reader holds its snapshot, so a loaded machine can't fail a test's
/// write on a read in flight.
#[test]
fn test_device_store_commits_while_a_reader_is_open() {
    let root = tempfile::tempdir().unwrap();
    let store = DeviceStore::open(root.path());
    let ns = "app".to_owned();
    let owner = vec![1; 32];
    assert!(store.acquire(ns.clone(), owner.clone()).unwrap());
    let reader = connect(&root.path().join("wallet.sqlite"));
    reader.execute_batch("BEGIN").unwrap();
    let before: i64 = reader
        .query_row("SELECT count(*) FROM wallet", [], |row| row.get(0))
        .unwrap();
    let put = vec![StoreWrite::Put {
        key: vec![1],
        value: vec![1],
    }];
    assert!(matches!(store.commit_if(ns.clone(), owner, put), Ok(true)));
    // The reader keeps the snapshot it began with.
    let during: i64 = reader
        .query_row("SELECT count(*) FROM wallet", [], |row| row.get(0))
        .unwrap();
    assert_eq!(during, before);
    reader.execute_batch("COMMIT").unwrap();
    assert_eq!(store.get(ns, vec![1]).unwrap(), Some(vec![1]));
}

#[test]
fn test_device_store_fences_a_second_process_and_a_reopened_writer() {
    const ROOT: &str = "PAYKIT_TEST_ENROLLMENT_STORE";
    let ns = "app".to_owned();
    let old = vec![1; 32];
    let new = vec![2; 32];
    let put = |value| {
        vec![StoreWrite::Put {
            key: vec![1],
            value: vec![value],
        }]
    };
    if let Some(path) = std::env::var_os(ROOT) {
        let second = DeviceStore::open(Path::new(&path));
        assert!(!second.acquire(ns.clone(), new.clone()).unwrap());
        assert!(!second.commit_if(ns.clone(), new, put(2)).unwrap());
        assert_eq!(second.get(ns, vec![1]).unwrap(), Some(vec![1]));
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let device = root.path().join("device");
    std::fs::create_dir(&device).unwrap();
    #[cfg(unix)]
    let other_path = {
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&device, &alias).unwrap();
        alias
    };
    #[cfg(not(unix))]
    let other_path = device.clone();
    let original = DeviceStore::open(&device);
    assert!(original.acquire(ns.clone(), old.clone()).unwrap());
    assert!(original.commit_if(ns.clone(), old.clone(), put(1)).unwrap());
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .env(ROOT, &other_path)
        .args([
            "--exact",
            "tests::storage::test_device_store_fences_a_second_process_and_a_reopened_writer",
        ])
        .output()
        .unwrap();
    assert!(
        child.status.success(),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(String::from_utf8_lossy(&child.stdout).contains("1 passed; 0 failed"));

    // A child starting while the host unlocks can briefly inherit the open file
    // description. A successful release must free the physical lock immediately.
    let inherited = original.leases.lock().unwrap()[&ns].1.try_clone().unwrap();
    original.release(ns.clone(), old.clone()).unwrap();
    let reopened = DeviceStore::open(&other_path);
    assert!(reopened.acquire(ns.clone(), new.clone()).unwrap());
    original.release(ns.clone(), old.clone()).unwrap();
    assert!(!original.commit_if(ns.clone(), old, put(1)).unwrap());
    assert!(reopened.commit_if(ns.clone(), new.clone(), put(2)).unwrap());
    assert_eq!(original.get(ns.clone(), vec![1]).unwrap(), Some(vec![2]));
    reopened.release(ns, new).unwrap();
    drop(inherited);
}
