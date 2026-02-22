use std::fmt;

/// Memory-crate errors.
#[derive(Debug)]
pub enum MemoryError {
    /// Filesystem I/O failure.
    Io(std::io::Error),
    /// JSON serialization/deserialization failure.
    Serde(serde_json::Error),
    /// Data integrity issue (e.g. unrecoverable corrupt JSONL).
    Corrupt(String),
    /// Dolt backend error.
    Dolt(String),
    /// Beads backend error.
    Beads(String),
    /// Backend not available (e.g. Dolt not configured, bd not on PATH).
    Unavailable(String),
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Serde(e) => write!(f, "serialization error: {e}"),
            Self::Corrupt(msg) => write!(f, "corrupt data: {msg}"),
            Self::Dolt(msg) => write!(f, "Dolt error: {msg}"),
            Self::Beads(msg) => write!(f, "Beads error: {msg}"),
            Self::Unavailable(msg) => write!(f, "backend unavailable: {msg}"),
        }
    }
}

impl std::error::Error for MemoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Serde(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for MemoryError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for MemoryError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serde(err)
    }
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
