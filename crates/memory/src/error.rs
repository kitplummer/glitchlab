/// Memory-crate errors.
#[derive(thiserror::Error, Debug)]
pub enum MemoryError {
    /// Filesystem I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization/deserialization failure.
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// Data integrity issue (e.g. unrecoverable corrupt JSONL).
    #[error("corrupt data: {0}")]
    Corrupt(String),
    /// Dolt backend error.
    #[error("Dolt error: {0}")]
    Dolt(String),
    /// Beads backend error.
    #[error("Beads error: {0}")]
    Beads(String),
    /// Backend not available (e.g. Dolt not configured, bd not on PATH).
    #[error("backend unavailable: {0}")]
    Unavailable(String),
}

/// Convenience alias.
pub type Result<T> = std::result::Result<T, MemoryError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_io() {
        let err = MemoryError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert!(err.to_string().contains("I/O error"));
        assert!(err.to_string().contains("gone"));
    }

    #[test]
    fn display_serde() {
        let bad: std::result::Result<serde_json::Value, _> = serde_json::from_str("{invalid");
        let err = MemoryError::from(bad.unwrap_err());
        assert!(err.to_string().contains("serialization error"));
    }

    #[test]
    fn display_corrupt() {
        let err = MemoryError::Corrupt("bad checksum".into());
        assert!(err.to_string().contains("corrupt data"));
        assert!(err.to_string().contains("bad checksum"));
    }

    #[test]
    fn display_dolt() {
        let err = MemoryError::Dolt("connection refused".into());
        assert!(err.to_string().contains("Dolt error"));
    }

    #[test]
    fn display_beads() {
        let err = MemoryError::Beads("bd not found".into());
        assert!(err.to_string().contains("Beads error"));
    }

    #[test]
    fn display_unavailable() {
        let err = MemoryError::Unavailable("no backend".into());
        assert!(err.to_string().contains("backend unavailable"));
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope");
        let err: MemoryError = io_err.into();
        assert!(matches!(err, MemoryError::Io(_)));
    }

    #[test]
    fn from_serde_error() {
        let serde_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let err: MemoryError = serde_err.into();
        assert!(matches!(err, MemoryError::Serde(_)));
    }

    #[test]
    fn error_source_io() {
        let err = MemoryError::Io(std::io::Error::other("oops"));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn error_source_serde() {
        let serde_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let err = MemoryError::Serde(serde_err);
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn error_source_none_for_string_variants() {
        let err = MemoryError::Corrupt("x".into());
        assert!(std::error::Error::source(&err).is_none());
    }
}
