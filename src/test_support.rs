//! Fixtures shared by the unit tests of several modules.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// A fresh directory under the OS temp dir for one test, removed with
/// everything in it on drop, also when an assertion fails.
pub struct ScratchDir(PathBuf);

impl ScratchDir {
    pub fn new(test_name: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rigbat-test-{test_name}-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("creating a scratch directory");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.0.join(path)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
