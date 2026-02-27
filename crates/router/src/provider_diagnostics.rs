#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCategory {
    /// The error is due to an invalid request from the client.
    BadRequest,
    /// The error is due to an issue with the provider's service.
    ProviderInternal,
    /// The error is due to a network issue.
    Network,
    /// The error is due to an unknown or unhandled issue.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDiagnostic {
    pub provider_name: String,
    pub model_name: String,
    pub error_category: ErrorCategory,
    pub error_message: String,
    pub status_code: Option<u16>,
}

pub fn classify_error(status_code: Option<u16>) -> ErrorCategory {
    match status_code {
        Some(code) if (400..=499).contains(&code) => ErrorCategory::BadRequest,
        Some(code) if (500..=599).contains(&code) => ErrorCategory::ProviderInternal,
        None => ErrorCategory::Network,
        _ => ErrorCategory::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_diagnostic_creation() {
        let diagnostic = ProviderDiagnostic {
            provider_name: "test_provider".to_string(),
            model_name: "test_model".to_string(),
            error_category: ErrorCategory::BadRequest,
            error_message: "Invalid request".to_string(),
            status_code: Some(400),
        };

        assert_eq!(diagnostic.provider_name, "test_provider");
        assert_eq!(diagnostic.model_name, "test_model");
        assert_eq!(diagnostic.error_category, ErrorCategory::BadRequest);
        assert_eq!(diagnostic.error_message, "Invalid request");
        assert_eq!(diagnostic.status_code, Some(400));
    }

    #[test]
    fn test_error_category_variants() {
        assert_eq!(format!("{:?}", ErrorCategory::BadRequest), "BadRequest");
        assert_eq!(
            format!("{:?}", ErrorCategory::ProviderInternal),
            "ProviderInternal"
        );
        assert_eq!(format!("{:?}", ErrorCategory::Network), "Network");
        assert_eq!(format!("{:?}", ErrorCategory::Unknown), "Unknown");
    }

    #[test]
    fn test_classify_error() {
        assert_eq!(
            classify_error(Some(400)),
            ErrorCategory::BadRequest,
            "Status code 400 should be BadRequest"
        );
        assert_eq!(
            classify_error(Some(404)),
            ErrorCategory::BadRequest,
            "Status code 404 should be BadRequest"
        );
        assert_eq!(
            classify_error(Some(499)),
            ErrorCategory::BadRequest,
            "Status code 499 should be BadRequest"
        );

        assert_eq!(
            classify_error(Some(500)),
            ErrorCategory::ProviderInternal,
            "Status code 500 should be ProviderInternal"
        );
        assert_eq!(
            classify_error(Some(503)),
            ErrorCategory::ProviderInternal,
            "Status code 503 should be ProviderInternal"
        );
        assert_eq!(
            classify_error(Some(599)),
            ErrorCategory::ProviderInternal,
            "Status code 599 should be ProviderInternal"
        );

        assert_eq!(
            classify_error(None),
            ErrorCategory::Network,
            "None status code should be Network error"
        );

        assert_eq!(
            classify_error(Some(200)),
            ErrorCategory::Unknown,
            "Status code 200 should be Unknown"
        );
        assert_eq!(
            classify_error(Some(302)),
            ErrorCategory::Unknown,
            "Status code 302 should be Unknown"
        );
        assert_eq!(
            classify_error(Some(101)),
            ErrorCategory::Unknown,
            "Status code 101 should be Unknown"
        );
    }
}
