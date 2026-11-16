/// Updates sent from the sync engine to whoever is listening.
#[derive(Debug, Clone)]
pub enum StateUpdate {
    /// Sync progress update
    SyncProgress {
        account: String,
        stage: SyncStage,
        progress: f32,
    },

    /// Retry attempt notification
    RetryAttempt {
        operation: String,
        attempt: usize,
        max_attempts: usize,
        next_delay_ms: u64,
        error: String,
    },

    /// IDLE detected external change in a folder (from another client/instance)
    ExternalChange { account: String, folder: String },
}

/// Stages of sync operation
#[derive(Debug, Clone)]
pub enum SyncStage {
    Connecting,
    FetchingFolders,
    SyncingFolder {
        name: String,
        /// Current message being processed (1-indexed for display)
        current: Option<usize>,
        /// Total messages to process in this folder
        total: Option<usize>,
    },
    ProcessingDeletions,
    Complete,
}
