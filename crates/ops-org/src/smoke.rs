use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{DeployTarget, SmokeTestEndpoint};

// ---------------------------------------------------------------------------
// HttpClient trait — enables mock injection
// ---------------------------------------------------------------------------

/// Response from an HTTP request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub latency_ms: u64,
    /// If the request itself failed (connection error, timeout), this is set.
    pub error: Option<String>,
}

/// Abstraction over HTTP requests for testability.
pub trait HttpClient: Send + Sync {
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = HttpResponse> + Send + 'a>>;
}

// ---------------------------------------------------------------------------
// ReqwestClient — production impl
// ---------------------------------------------------------------------------

/// Production HTTP client using `reqwest`.
pub struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for ReqwestClient {
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = HttpResponse> + Send + 'a>> {
        Box::pin(async move {
            let start = std::time::Instant::now();
            let req_method = match method.to_uppercase().as_str() {
                "GET" => reqwest::Method::GET,
                "POST" => reqwest::Method::POST,
                "PUT" => reqwest::Method::PUT,
                "DELETE" => reqwest::Method::DELETE,
                _ => reqwest::Method::GET,
            };

            match self
                .client
                .request(req_method, url)
                .timeout(timeout)
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let body = resp.text().await.unwrap_or_default();
                    HttpResponse {
                        status,
                        body,
                        latency_ms: start.elapsed().as_millis() as u64,
                        error: None,
                    }
                }
                Err(e) => HttpResponse {
                    status: 0,
                    body: String::new(),
                    latency_ms: start.elapsed().as_millis() as u64,
                    error: Some(e.to_string()),
                },
            }
        })
    }
}

// ---------------------------------------------------------------------------
// SmokeCheckResult / SmokeTestReport
// ---------------------------------------------------------------------------

/// Result of a single smoke test check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmokeCheckResult {
    pub endpoint: String,
    pub method: String,
    pub expected_status: u16,
    pub actual_status: u16,
    pub passed: bool,
    pub required: bool,
    pub detail: String,
    pub latency_ms: u64,
}

/// Aggregate report of all smoke tests for a deploy target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmokeTestReport {
    pub target: String,
    pub checks: Vec<SmokeCheckResult>,
    pub all_required_passed: bool,
    pub all_passed: bool,
    pub summary: String,
}

impl SmokeTestReport {
    /// Format as text suitable for injection into the Uplink agent context.
    pub fn as_health_results_text(&self) -> String {
        let mut lines = Vec::new();
        for check in &self.checks {
            let status_icon = if check.passed { "PASS" } else { "FAIL" };
            let req_marker = if check.required {
                "[required]"
            } else {
                "[optional]"
            };
            lines.push(format!(
                "{status_icon} {req_marker} {} {} → {} (expected {}, {}ms): {}",
                check.method,
                check.endpoint,
                check.actual_status,
                check.expected_status,
                check.latency_ms,
                check.detail,
            ));
        }
        lines.push(String::new());
        lines.push(self.summary.clone());
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Evaluation logic
// ---------------------------------------------------------------------------

/// Evaluate a single smoke check against the endpoint definition and HTTP response.
///
/// Returns `(passed, detail_message)`.
pub fn evaluate_check(endpoint: &SmokeTestEndpoint, response: &HttpResponse) -> (bool, String) {
    // Connection / request error
    if let Some(ref err) = response.error {
        return (false, format!("request error: {err}"));
    }

    // Status code mismatch
    if response.status != endpoint.expected_status {
        return (
            false,
            format!(
                "expected status {}, got {}",
                endpoint.expected_status, response.status
            ),
        );
    }

    // Body check
    if let Some(ref expected) = endpoint.expected_body_contains {
        if !response.body.contains(expected.as_str()) {
            return (
                false,
                format!("response body missing expected substring: {expected:?}"),
            );
        }
        return (
            true,
            format!("status {} OK, body contains {expected:?}", response.status),
        );
    }

    (true, format!("status {} OK", response.status))
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// Run all smoke tests for a deploy target.
pub async fn run_smoke_tests(
    target: &DeployTarget,
    client: &dyn HttpClient,
    timeout: Duration,
) -> SmokeTestReport {
    let mut checks = Vec::new();

    for endpoint in &target.smoke_tests {
        let url = format!("{}{}", target.base_url, endpoint.path);
        let response = client.request(&endpoint.method, &url, timeout).await;
        let (passed, detail) = evaluate_check(endpoint, &response);

        checks.push(SmokeCheckResult {
            endpoint: endpoint.path.clone(),
            method: endpoint.method.clone(),
            expected_status: endpoint.expected_status,
            actual_status: response.status,
            passed,
            required: endpoint.required,
            detail,
            latency_ms: response.latency_ms,
        });
    }

    let all_required_passed = checks.iter().filter(|c| c.required).all(|c| c.passed);
    let all_passed = checks.iter().all(|c| c.passed);

    let passed_count = checks.iter().filter(|c| c.passed).count();
    let total = checks.len();
    let summary = format!(
        "{passed_count}/{total} checks passed (all_required: {all_required_passed}, all: {all_passed})"
    );

    SmokeTestReport {
        target: target.app_name.clone(),
        checks,
        all_required_passed,
        all_passed,
        summary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SmokeTestEndpoint;
    use std::sync::Mutex;

    // Mock HTTP client for tests
    struct MockHttpClient {
        responses: Mutex<Vec<HttpResponse>>,
    }

    impl MockHttpClient {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl HttpClient for MockHttpClient {
        fn request<'a>(
            &'a self,
            _method: &'a str,
            _url: &'a str,
            _timeout: Duration,
        ) -> Pin<Box<dyn Future<Output = HttpResponse> + Send + 'a>> {
            let resp = self
                .responses
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(HttpResponse {
                    status: 500,
                    body: "no more mock responses".into(),
                    latency_ms: 0,
                    error: None,
                });
            Box::pin(async move { resp })
        }
    }

    fn endpoint(path: &str, status: u16, required: bool) -> SmokeTestEndpoint {
        SmokeTestEndpoint {
            method: "GET".into(),
            path: path.into(),
            expected_status: status,
            expected_body_contains: None,
            required,
        }
    }

    fn endpoint_with_body(
        path: &str,
        status: u16,
        body: &str,
        required: bool,
    ) -> SmokeTestEndpoint {
        SmokeTestEndpoint {
            method: "GET".into(),
            path: path.into(),
            expected_status: status,
            expected_body_contains: Some(body.into()),
            required,
        }
    }

    fn ok_response(status: u16) -> HttpResponse {
        HttpResponse {
            status,
            body: "OK".into(),
            latency_ms: 42,
            error: None,
        }
    }

    fn ok_response_with_body(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            body: body.into(),
            latency_ms: 42,
            error: None,
        }
    }

    fn error_response(msg: &str) -> HttpResponse {
        HttpResponse {
            status: 0,
            body: String::new(),
            latency_ms: 0,
            error: Some(msg.into()),
        }
    }

    // --- evaluate_check tests ---

    #[test]
    fn evaluate_check_status_match() {
        let ep = endpoint("/health", 200, true);
        let resp = ok_response(200);
        let (passed, detail) = evaluate_check(&ep, &resp);
        assert!(passed);
        assert!(detail.contains("200 OK"));
    }

    #[test]
    fn evaluate_check_status_mismatch() {
        let ep = endpoint("/health", 200, true);
        let resp = ok_response(503);
        let (passed, detail) = evaluate_check(&ep, &resp);
        assert!(!passed);
        assert!(detail.contains("expected status 200"));
        assert!(detail.contains("got 503"));
    }

    #[test]
    fn evaluate_check_body_match() {
        let ep = endpoint_with_body("/", 200, "LowEndInsight", true);
        let resp = ok_response_with_body(200, "Welcome to LowEndInsight API");
        let (passed, detail) = evaluate_check(&ep, &resp);
        assert!(passed);
        assert!(detail.contains("body contains"));
    }

    #[test]
    fn evaluate_check_body_mismatch() {
        let ep = endpoint_with_body("/", 200, "LowEndInsight", true);
        let resp = ok_response_with_body(200, "Some other content");
        let (passed, detail) = evaluate_check(&ep, &resp);
        assert!(!passed);
        assert!(detail.contains("missing expected substring"));
    }

    #[test]
    fn evaluate_check_connection_error() {
        let ep = endpoint("/health", 200, true);
        let resp = error_response("connection refused");
        let (passed, detail) = evaluate_check(&ep, &resp);
        assert!(!passed);
        assert!(detail.contains("request error"));
        assert!(detail.contains("connection refused"));
    }

    #[test]
    fn evaluate_check_body_not_checked_on_wrong_status() {
        let ep = endpoint_with_body("/", 200, "LowEndInsight", true);
        let resp = ok_response_with_body(500, "LowEndInsight error page");
        let (passed, detail) = evaluate_check(&ep, &resp);
        assert!(!passed);
        assert!(detail.contains("expected status 200"));
    }

    // --- run_smoke_tests tests ---

    #[tokio::test]
    async fn smoke_tests_all_pass() {
        let target = DeployTarget {
            app_name: "test-app".into(),
            base_url: "https://test.fly.dev".into(),
            smoke_tests: vec![endpoint("/", 200, true), endpoint("/health", 200, false)],
            fly_toml_path: None,
            fly_org: None,
        };
        // MockHttpClient pops from back, so push in reverse order
        let client = MockHttpClient::new(vec![ok_response(200), ok_response(200)]);
        let report = run_smoke_tests(&target, &client, Duration::from_secs(5)).await;

        assert!(report.all_required_passed);
        assert!(report.all_passed);
        assert_eq!(report.checks.len(), 2);
        assert!(report.summary.contains("2/2"));
    }

    #[tokio::test]
    async fn smoke_tests_required_fails() {
        let target = DeployTarget {
            app_name: "test-app".into(),
            base_url: "https://test.fly.dev".into(),
            smoke_tests: vec![endpoint("/", 200, true), endpoint("/health", 200, false)],
            fly_toml_path: None,
            fly_org: None,
        };
        // Required endpoint gets 503, optional gets 200
        let client = MockHttpClient::new(vec![ok_response(200), ok_response(503)]);
        let report = run_smoke_tests(&target, &client, Duration::from_secs(5)).await;

        assert!(!report.all_required_passed);
        assert!(!report.all_passed);
        assert!(report.summary.contains("1/2"));
    }

    #[tokio::test]
    async fn smoke_tests_optional_fails() {
        let target = DeployTarget {
            app_name: "test-app".into(),
            base_url: "https://test.fly.dev".into(),
            smoke_tests: vec![endpoint("/", 200, true), endpoint("/doc", 200, false)],
            fly_toml_path: None,
            fly_org: None,
        };
        // Required passes, optional fails
        let client = MockHttpClient::new(vec![ok_response(404), ok_response(200)]);
        let report = run_smoke_tests(&target, &client, Duration::from_secs(5)).await;

        assert!(report.all_required_passed);
        assert!(!report.all_passed);
    }

    #[tokio::test]
    async fn smoke_tests_connection_error() {
        let target = DeployTarget {
            app_name: "test-app".into(),
            base_url: "https://test.fly.dev".into(),
            smoke_tests: vec![endpoint("/", 200, true)],
            fly_toml_path: None,
            fly_org: None,
        };
        let client = MockHttpClient::new(vec![error_response("timeout")]);
        let report = run_smoke_tests(&target, &client, Duration::from_secs(5)).await;

        assert!(!report.all_required_passed);
        assert!(!report.all_passed);
        assert!(!report.checks[0].passed);
    }

    // --- report formatting ---

    #[test]
    fn report_as_health_results_text() {
        let report = SmokeTestReport {
            target: "test-app".into(),
            checks: vec![
                SmokeCheckResult {
                    endpoint: "/".into(),
                    method: "GET".into(),
                    expected_status: 200,
                    actual_status: 200,
                    passed: true,
                    required: true,
                    detail: "status 200 OK".into(),
                    latency_ms: 42,
                },
                SmokeCheckResult {
                    endpoint: "/doc".into(),
                    method: "GET".into(),
                    expected_status: 200,
                    actual_status: 404,
                    passed: false,
                    required: false,
                    detail: "expected status 200, got 404".into(),
                    latency_ms: 15,
                },
            ],
            all_required_passed: true,
            all_passed: false,
            summary: "1/2 checks passed (all_required: true, all: false)".into(),
        };

        let text = report.as_health_results_text();
        assert!(text.contains("PASS [required] GET /"));
        assert!(text.contains("FAIL [optional] GET /doc"));
        assert!(text.contains("1/2 checks passed"));
    }

    #[test]
    fn smoke_check_result_serde_roundtrip() {
        let result = SmokeCheckResult {
            endpoint: "/test".into(),
            method: "POST".into(),
            expected_status: 200,
            actual_status: 200,
            passed: true,
            required: true,
            detail: "ok".into(),
            latency_ms: 10,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: SmokeCheckResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.endpoint, "/test");
        assert!(parsed.passed);
    }

    #[test]
    fn smoke_test_report_serde_roundtrip() {
        let report = SmokeTestReport {
            target: "app".into(),
            checks: vec![],
            all_required_passed: true,
            all_passed: true,
            summary: "0/0".into(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let parsed: SmokeTestReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.target, "app");
    }

    #[test]
    fn http_response_serde_roundtrip() {
        let resp = HttpResponse {
            status: 200,
            body: "hello".into(),
            latency_ms: 100,
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HttpResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, 200);
    }

    #[test]
    fn reqwest_client_construction() {
        let client = ReqwestClient::new();
        let _ = &client; // ensure it can be constructed
        let client2 = ReqwestClient::default();
        let _ = &client2;
    }

    #[tokio::test]
    async fn smoke_tests_empty_endpoints() {
        let target = DeployTarget {
            app_name: "empty-app".into(),
            base_url: "https://test.fly.dev".into(),
            smoke_tests: vec![],
            fly_toml_path: None,
            fly_org: None,
        };
        let client = MockHttpClient::new(vec![]);
        let report = run_smoke_tests(&target, &client, Duration::from_secs(5)).await;

        assert!(report.all_required_passed);
        assert!(report.all_passed);
        assert_eq!(report.checks.len(), 0);
        assert!(report.summary.contains("0/0"));
    }

    #[test]
    fn http_response_with_error() {
        let resp = HttpResponse {
            status: 0,
            body: String::new(),
            latency_ms: 0,
            error: Some("timeout".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: HttpResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.error, Some("timeout".into()));
    }
}
