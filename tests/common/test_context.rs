//! Isolated test context for proper test isolation

use std::path::PathBuf;
use tempfile::TempDir;

use orb_core::Database;

/// Provides isolated file system context for tests
///
/// Each test gets its own temporary directory containing:
/// - Database file
/// - Config file
/// - Any other test artifacts
#[allow(dead_code)]
pub struct TestContext {
    pub temp_dir: TempDir,
    pub db_path: PathBuf,
    pub config_path: PathBuf,
}

impl TestContext {
    /// Create a new isolated test context
    pub fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let db_path = temp_dir.path().join("test.db");
        let config_path = temp_dir.path().join("config.yml");

        Self {
            temp_dir,
            db_path,
            config_path,
        }
    }

    /// Create and return a database instance using the test database path
    pub fn with_database(&self) -> Database {
        let path_str = self.db_path.to_str().expect("Invalid path");
        Database::new(path_str).expect("Failed to create test database")
    }

    /// Get a path within the temp directory
    #[allow(dead_code)]
    pub fn path(&self, name: &str) -> PathBuf {
        self.temp_dir.path().join(name)
    }
}

impl Default for TestContext {
    fn default() -> Self {
        Self::new()
    }
}
