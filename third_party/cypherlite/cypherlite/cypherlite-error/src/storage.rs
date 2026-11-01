use std::fmt;

/// Errors that can occur in the storage layer.
#[derive(Debug, Clone, PartialEq)]
pub enum StorageError {
    /// A referenced node was not found.
    NodeNotFound(i64),
    /// A referenced relationship was not found.
    RelationshipNotFound(i64),
    /// Cannot delete a node that still has relationships.
    NodeHasRelationships(i64),
    /// A uniqueness or existence constraint was violated.
    ConstraintViolation(String),
    /// An index with the given name already exists.
    IndexAlreadyExists(String),
    /// No index found with the given name.
    IndexNotFound(String),
    /// A constraint with the given name already exists.
    ConstraintAlreadyExists(String),
    /// No constraint found with the given name.
    ConstraintNotFound(String),
    /// The selected backend cannot provide transactional semantics.
    TransactionsUnsupported,
    /// A transaction is already active on this store.
    TransactionAlreadyActive,
    /// No transaction is active on this store.
    NoActiveTransaction,
    /// Persistence was published before a rollback-restoration failure made the durable outcome unknowable.
    /// The store is poisoned and must not be used or reopened until its files are explicitly discarded.
    PersistenceOutcomeIndeterminate(String),
    /// A filesystem / I/O failure (open, read, write, sync, rename, remove).
    ///
    /// `io::Error` is neither `Clone` nor `PartialEq`, so the raw error is not
    /// carried; instead the classifying [`std::io::ErrorKind`] is kept
    /// alongside a fully-rendered message (which already embeds the original
    /// error's text, so `Display` is unchanged from the old `Other` string).
    Io {
        kind: std::io::ErrorKind,
        message: String,
    },
    /// A database file could not be decoded: its stored bytes are not the
    /// expected on-disk format (e.g. malformed JSON). Distinct from [`Io`],
    /// which is a filesystem failure; a `Corruption` means the file was read
    /// but its contents are unusable.
    Corruption(String),
    /// A requested operation conflicts with the store's current state (e.g.
    /// flushing while an explicit transaction is still open).
    Conflict(String),
    /// The database file is already open (exclusively locked) by another live
    /// process. Carries the database path. The store refuses to open rather
    /// than risk two writers clobbering each other; there is no read-only
    /// fallback.
    Locked(String),
    /// A mutation was attempted against a read-only snapshot of the store —
    /// today, a backend checked out to a historical MVCC version. Carries a
    /// human-readable explanation. The store rejects the write rather than
    /// silently destroying history by overwriting HEAD with the snapshot.
    ReadOnly(String),
    /// A generic error that fits no more specific variant.
    Other(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::NodeNotFound(id) => write!(f, "node {id} not found"),
            StorageError::RelationshipNotFound(id) => {
                write!(f, "relationship {id} not found")
            }
            StorageError::NodeHasRelationships(id) => write!(
                f,
                "cannot delete node {id} with existing relationships (use DETACH DELETE)"
            ),
            StorageError::ConstraintViolation(msg) => {
                write!(f, "constraint violation: {msg}")
            }
            StorageError::IndexAlreadyExists(name) => {
                write!(f, "index {name:?} already exists")
            }
            StorageError::IndexNotFound(name) => write!(f, "index {name:?} not found"),
            StorageError::ConstraintAlreadyExists(name) => {
                write!(f, "constraint {name:?} already exists")
            }
            StorageError::ConstraintNotFound(name) => {
                write!(f, "constraint {name:?} not found")
            }
            StorageError::TransactionsUnsupported => {
                write!(f, "transactions are not supported by this storage backend")
            }
            StorageError::TransactionAlreadyActive => write!(f, "a transaction is already active"),
            StorageError::NoActiveTransaction => write!(f, "no transaction is active"),
            StorageError::PersistenceOutcomeIndeterminate(msg) => {
                write!(f, "persistence outcome is indeterminate: {msg}")
            }
            // `Io`/`Corruption`/`Conflict` render their message verbatim: the
            // message already carries all context (it was formatted at the
            // failing call site), so byte-for-byte Display is preserved from
            // the pre-refactor `Other(...)` strings.
            StorageError::Io { message, .. } => write!(f, "{message}"),
            StorageError::Corruption(msg) => write!(f, "{msg}"),
            StorageError::Conflict(msg) => write!(f, "{msg}"),
            StorageError::Locked(path) => {
                write!(f, "database is locked by another process: {path}")
            }
            StorageError::ReadOnly(msg) => write!(f, "{msg}"),
            StorageError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for StorageError {}

impl StorageError {
    /// Build an [`StorageError::Io`] from a raw [`std::io::Error`], capturing
    /// its [`std::io::ErrorKind`] and a caller-provided message that already
    /// embeds the error text.
    pub fn io(error: &std::io::Error, message: String) -> Self {
        StorageError::Io {
            kind: error.kind(),
            message,
        }
    }
}
