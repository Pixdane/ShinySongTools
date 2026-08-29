//! Base value types: stable ids and the game data root.

use std::path::PathBuf;

/// Stable id of a plugin owner scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OwnerId(pub u64);

/// Stable id of a cross-domain message route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RouteId(pub u64);

/// Stable id of a debug topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TopicId(pub u64);

/// The game sandbox Documents directory: the only writable root the runtime
/// reads configuration from and creates its files under. Never a project
/// root, never a `.app` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataRoot(PathBuf);

impl DataRoot {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }

    #[must_use]
    pub fn path(&self) -> &PathBuf {
        &self.0
    }

    /// Resolve `relative` under this root.
    #[must_use]
    pub fn join(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }
}

impl core::fmt::Display for DataRoot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never print concrete user paths in logs.
        f.write_str("DataRoot(<documents>)")
    }
}
