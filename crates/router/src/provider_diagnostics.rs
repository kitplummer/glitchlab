//! Provider diagnostics module
use http::StatusCode;

#[derive(Debug, PartialEq)]
pub enum ErrorCategory {
    BadGateway,
    BadRequest,
    Conflict,
    Forbidden,
    Internal,
    NotFound,
    ServiceUnavailable,
    TooManyRequests,
    Unauthorized,
    Unprocessable,
    UnsupportedMediaType,
    Other,
}

#[derive(Debug, PartialEq)]
pub struct ErrorInfo {
    pub category: ErrorCategory,
    pub retryable: bool,
}

pub fn classify_error(status_code: StatusCode) -> ErrorInfo {
    match status_code {
        // 4xx Client Errors
        StatusCode::BAD_REQUEST => ErrorInfo {
            category: ErrorCategory::BadRequest,
            retryable: false,
        },
        StatusCode::UNAUTHORIZED => ErrorInfo {
            category: ErrorCategory::Unauthorized,
            retryable: false,
        },
        StatusCode::FORBIDDEN => ErrorInfo {
            category: ErrorCategory::Forbidden,
            retryable: false,
        },
        StatusCode::NOT_FOUND => ErrorInfo {
            category: ErrorCategory::NotFound,
            retryable: false,
        },
        StatusCode::CONFLICT => ErrorInfo {
            category: ErrorCategory::Conflict,
            retryable: false,
        },
        StatusCode::UNPROCESSABLE_ENTITY => ErrorInfo {
            category: ErrorCategory::Unprocessable,
            retryable: false,
        },
        StatusCode::TOO_MANY_REQUESTS => ErrorInfo {
            category: ErrorCategory::TooManyRequests,
            retryable: true,
        },
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ErrorInfo {
            category: ErrorCategory::UnsupportedMediaType,
            retryable: false,
        },
        // 5xx Server Errors
        StatusCode::INTERNAL_SERVER_ERROR => ErrorInfo {
            category: ErrorCategory::Internal,
            retryable: true,
        },
        StatusCode::BAD_GATEWAY => ErrorInfo {
            category: ErrorCategory::BadGateway,
            retryable: true,
        },
        StatusCode::SERVICE_UNAVAILABLE => ErrorInfo {
            category: ErrorCategory::ServiceUnavailable,
            retryable: true,
        },
        _ => ErrorInfo {
            category: ErrorCategory::Other,
            retryable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_error_bad_request() {
        let error_info = classify_error(StatusCode::BAD_REQUEST);
        assert_eq!(error_info.category, ErrorCategory::BadRequest);
        assert!(!error_info.retryable);
    }

    #[test]
    fn test_classify_error_internal_server_error() {
        let error_info = classify_error(StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error_info.category, ErrorCategory::Internal);
        assert!(error_info.retryable);
    }

    #[test]
    fn test_classify_error_too_many_requests() {
        let error_info = classify_error(StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(error_info.category, ErrorCategory::TooManyRequests);
        assert!(error_info.retryable);
    }

    #[test]
    fn test_classify_error_other() {
        let error_info = classify_error(StatusCode::IM_A_TEAPOT);
        assert_eq!(error_info.category, ErrorCategory::Other);
        assert!(!error_info.retryable);
    }
}
