//! Retry time; the test override is scoped to one synthetic SQLite database.
//! Overrides are process-local, shared by file-backed connections by canonical
//! path, and do not apply to in-memory connections or subprocesses.
use std::path::Path;

pub(crate) fn now_for_path(_path: &Path) -> i64 {
    #[cfg(test)]
    if let Some(now) = test_clock::get(_path) {
        return now;
    }
    chrono::Utc::now().timestamp()
}

pub(super) fn now_for_connection(_conn: &rusqlite::Connection) -> i64 {
    #[cfg(test)]
    if let Some(path) = _conn.path() {
        return now_for_path(Path::new(path));
    }
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
pub(crate) mod test_clock {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{LazyLock, Mutex},
    };
    static CLOCKS: LazyLock<Mutex<HashMap<PathBuf, i64>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    fn key(path: &Path) -> PathBuf {
        path.canonicalize()
            .unwrap_or_else(|_| match (path.parent(), path.file_name()) {
                (Some(parent), Some(name)) => parent
                    .canonicalize()
                    .unwrap_or_else(|_| parent.to_path_buf())
                    .join(name),
                _ => path.to_path_buf(),
            })
    }
    pub(super) fn get(path: &Path) -> Option<i64> {
        CLOCKS.lock().unwrap().get(&key(path)).copied()
    }
    pub(crate) struct Control {
        path: PathBuf,
    }
    impl Control {
        pub(crate) fn install(path: &Path, now: i64) -> Self {
            assert!(
                path.is_absolute(),
                "test clock requires an absolute database path"
            );
            let path = key(path);
            let mut clocks = CLOCKS.lock().unwrap();
            if clocks.contains_key(&path) {
                drop(clocks);
                panic!("test clock already installed");
            }
            clocks.insert(path.clone(), now);
            Self { path }
        }
        pub(crate) fn set(&self, now: i64) {
            *CLOCKS
                .lock()
                .unwrap()
                .get_mut(&self.path)
                .expect("test clock missing") = now;
        }
    }
    impl Drop for Control {
        fn drop(&mut self) {
            CLOCKS.lock().unwrap().remove(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{now_for_connection, now_for_path, test_clock::Control};
    #[test]
    fn duplicate_install_does_not_poison_other_clocks() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.db");
        let second = root.path().join("second.db");
        let first_clock = Control::install(&first, 1234);
        assert!(std::panic::catch_unwind(|| Control::install(&first, 5678)).is_err());
        let second_clock = Control::install(&second, 4321);
        assert_eq!(now_for_path(&first), 1234);
        assert_eq!(now_for_path(&second), 4321);
        drop(first_clock);
        drop(second_clock);
    }

    #[test]
    fn clock_is_per_database_and_survives_reopen() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.db");
        let second = root.path().join("second.db");
        let clock = Control::install(&first, 1234);
        let a = rusqlite::Connection::open(&first).unwrap();
        let b = rusqlite::Connection::open(&second).unwrap();
        assert_eq!(now_for_connection(&a), 1234);
        assert_ne!(now_for_connection(&b), 1234);
        clock.set(2345);
        drop(a);
        let reopened = rusqlite::Connection::open(&first).unwrap();
        assert_eq!(now_for_connection(&reopened), 2345);
        drop(clock);
        assert_ne!(now_for_path(&first), 2345);
    }
}
