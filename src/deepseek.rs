use std::time::Duration;

use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::config::{DEFAULT_ENDPOINT, HTTP_TIMEOUT, MAX_RESPONSE_BODY_BYTES, MAX_RETRIES};
use crate::error::{AppError, UpstreamError};

const SEARCH_PATH: &str = "/v1/tools/search";
const FETCH_PATH: &str = "/v1/tools/fetch";
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];

#[derive(Clone)]
pub struct DeepSeekClient {
    http: Client,
    endpoint: String,
    api_key: String,
}

#[derive(Debug, Serialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ori_question: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub localization: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub need_content: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct FetchRequest {
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Deserialize)]
pub struct SearchResult {
    pub rerank_score: Option<f64>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub snippet: Option<String>,
    pub summary: Option<String>,
    pub content: Option<String>,
    pub site_name: Option<String>,
    pub site_icon: Option<String>,
    pub date: Option<String>,
    pub authority: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct FetchResponse {
    pub title: Option<String>,
    pub url: Option<String>,
    pub content: String,
}

impl DeepSeekClient {
    pub fn new(config: &crate::config::Config) -> Result<Self, AppError> {
        Self::build(config, DEFAULT_ENDPOINT.to_owned())
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        config: &crate::config::Config,
        endpoint: String,
    ) -> Result<Self, AppError> {
        Self::build(config, endpoint)
    }

    fn build(config: &crate::config::Config, endpoint: String) -> Result<Self, AppError> {
        let http = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|error| AppError::Transport {
                detail: format!("failed to build HTTP client: {error}"),
            })?;
        Ok(Self {
            http,
            endpoint: endpoint.trim_end_matches('/').to_owned(),
            api_key: config.api_key().to_owned(),
        })
    }

    pub async fn search(
        &self,
        request: SearchRequest,
        cancellation: &CancellationToken,
    ) -> Result<SearchResponse, AppError> {
        let response = self.post_json(SEARCH_PATH, &request, cancellation).await?;
        parse_json_response(response, "search", cancellation).await
    }

    pub async fn fetch(
        &self,
        request: FetchRequest,
        cancellation: &CancellationToken,
    ) -> Result<FetchResponse, AppError> {
        let response = self.post_json(FETCH_PATH, &request, cancellation).await?;
        let parsed: FetchResponse = parse_json_response(response, "fetch", cancellation).await?;
        if parsed.content.trim().is_empty() {
            return Err(AppError::Upstream(UpstreamError::Protocol {
                detail: "fetch response content was empty".to_owned(),
            }));
        }
        Ok(parsed)
    }

    async fn post_json<T: Serialize>(
        &self,
        path: &str,
        payload: &T,
        cancellation: &CancellationToken,
    ) -> Result<Response, AppError> {
        for attempt in 0..=MAX_RETRIES {
            let result = tokio::select! {
                _ = cancellation.cancelled() => return Err(cancelled_error()),
                result = self.send_once(path, payload) => result,
            };
            match result {
                Ok(response) if should_retry_status(response.status()) && attempt < MAX_RETRIES => {
                    wait_before_retry(attempt, cancellation).await?;
                }
                Ok(response) if should_retry_status(response.status()) => {
                    return Err(self.status_error(response, cancellation).await);
                }
                Ok(response) => return self.validate_status(response, cancellation).await,
                Err(error) if should_retry_transport(&error) && attempt < MAX_RETRIES => {
                    wait_before_retry(attempt, cancellation).await?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("retry loop always returns")
    }

    async fn send_once<T: Serialize>(&self, path: &str, payload: &T) -> Result<Response, AppError> {
        self.http
            .post(format!("{}{path}", self.endpoint))
            .bearer_auth(&self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .json(payload)
            .send()
            .await
            .map_err(|error| AppError::Transport {
                detail: format!("upstream request failed: {error}"),
            })
    }

    async fn validate_status(
        &self,
        response: Response,
        cancellation: &CancellationToken,
    ) -> Result<Response, AppError> {
        if response.status().is_success() {
            return Ok(response);
        }
        Err(self.status_error(response, cancellation).await)
    }

    async fn status_error(&self, response: Response, cancellation: &CancellationToken) -> AppError {
        let status = response.status().as_u16();
        let body = match tokio::select! {
            _ = cancellation.cancelled() => Err(cancelled_error()),
            body = bounded_body(response) => body,
        } {
            Ok(body) => body,
            Err(error) => return error,
        };
        let summary = upstream_error_message(&body, &self.api_key);
        if summary.is_empty() {
            AppError::Upstream(UpstreamError::HttpStatus {
                status,
                message: String::new(),
            })
        } else {
            AppError::Upstream(UpstreamError::HttpStatusSummary { status, summary })
        }
    }
}

async fn parse_json_response<T: for<'de> Deserialize<'de>>(
    response: Response,
    operation: &str,
    cancellation: &CancellationToken,
) -> Result<T, AppError> {
    let body = tokio::select! {
        _ = cancellation.cancelled() => return Err(cancelled_error()),
        body = bounded_body(response) => body?,
    };
    serde_json::from_slice(&body).map_err(|error| {
        AppError::Upstream(UpstreamError::Protocol {
            detail: format!("invalid {operation} response JSON: {error}"),
        })
    })
}

async fn bounded_body(mut response: Response) -> Result<Vec<u8>, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
    {
        return Err(AppError::Upstream(UpstreamError::Protocol {
            detail: "upstream response exceeded the maximum body size".to_owned(),
        }));
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| AppError::Transport {
            detail: format!("failed to read upstream response: {error}"),
        })?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BODY_BYTES {
            return Err(AppError::Upstream(UpstreamError::Protocol {
                detail: "upstream response exceeded the maximum body size".to_owned(),
            }));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn wait_before_retry(attempt: u8, cancellation: &CancellationToken) -> Result<(), AppError> {
    let delay = RETRY_DELAYS[usize::from(attempt).min(RETRY_DELAYS.len() - 1)];
    tokio::select! {
        _ = cancellation.cancelled() => Err(cancelled_error()),
        _ = tokio::time::sleep(delay) => Ok(()),
    }
}

fn should_retry_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

fn should_retry_transport(error: &AppError) -> bool {
    match error {
        AppError::Transport { detail } => {
            !detail.contains("timed out") && !detail.contains("deadline")
        }
        _ => false,
    }
}

fn cancelled_error() -> AppError {
    AppError::Cancelled
}

fn upstream_error_message(body: &[u8], api_key: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorEnvelope {
        error: Option<ErrorMessage>,
    }
    #[derive(Deserialize)]
    struct ErrorMessage {
        message: Option<String>,
    }

    let Some(message) = serde_json::from_slice::<ErrorEnvelope>(body)
        .ok()
        .and_then(|envelope| envelope.error)
        .and_then(|error| error.message)
    else {
        return String::new();
    };
    let sanitized = message.replace(api_key, "<redacted>");
    let lowercase = sanitized.to_ascii_lowercase();
    let sensitive_markers = [
        "authorization",
        "bearer",
        "password",
        "secret",
        "token",
        "api_key",
        "apikey",
        "credential",
    ];
    if sensitive_markers
        .iter()
        .any(|marker| lowercase.contains(marker))
        || sanitized.contains("http://")
        || sanitized.contains("https://")
        || sanitized.contains("www.")
        || sanitized.contains("://")
    {
        return "upstream rejected the request".to_owned();
    }
    sanitized
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, Request, Respond, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    use super::{DeepSeekClient, FetchRequest, SearchRequest};
    use crate::config::Config;
    use crate::error::ErrorCategory;

    fn client(server: &MockServer) -> DeepSeekClient {
        let config = Config::from_value(Some("test-secret")).expect("test config");
        DeepSeekClient::new_for_test(&config, server.uri()).expect("test client")
    }

    fn search_request() -> SearchRequest {
        SearchRequest {
            query: "rust async".to_owned(),
            count: Some(3),
            ori_question: Some("How does async Rust work?".to_owned()),
            localization: None,
            need_content: Some(false),
        }
    }

    #[tokio::test]
    async fn search_sends_only_provided_optional_fields_and_expected_headers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .and(header("authorization", "Bearer test-secret"))
            .and(header("content-type", "application/json"))
            .and(header("accept", "application/json"))
            .and(body_json(serde_json::json!({
                "query": "rust async",
                "count": 3,
                "ori_question": "How does async Rust work?",
                "need_content": false
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "results": []
            })))
            .expect(1)
            .mount(&server)
            .await;

        let response = client(&server)
            .search(search_request(), &CancellationToken::new())
            .await
            .expect("search should succeed");

        assert!(response.results.is_empty());
    }

    #[tokio::test]
    async fn fetch_sends_the_expected_body_and_parses_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/fetch"))
            .and(body_json(
                serde_json::json!({"url": "https://example.com/article"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "Example",
                "url": "https://example.com/article",
                "content": "# Article"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let response = client(&server)
            .fetch(
                FetchRequest {
                    url: "https://example.com/article".to_owned(),
                },
                &CancellationToken::new(),
            )
            .await
            .expect("fetch should succeed");

        assert_eq!(response.title.as_deref(), Some("Example"));
        assert_eq!(response.content, "# Article");
    }

    #[tokio::test]
    async fn validates_search_results_and_fetch_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": {}})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/fetch"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"title": "Empty", "content": ""})),
            )
            .mount(&server)
            .await;

        let api = client(&server);
        let search_error = api
            .search(search_request(), &CancellationToken::new())
            .await
            .expect_err("invalid results must fail");
        assert_eq!(search_error.category(), ErrorCategory::Upstream);
        let fetch_error = api
            .fetch(
                FetchRequest {
                    url: "https://example.com".to_owned(),
                },
                &CancellationToken::new(),
            )
            .await
            .expect_err("empty content must fail");
        assert_eq!(fetch_error.category(), ErrorCategory::Upstream);
    }

    #[tokio::test]
    async fn retries_transient_statuses_only_up_to_two_extra_attempts() {
        let server = MockServer::start().await;
        let attempts = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(CountingResponder {
                attempts: Arc::clone(&attempts),
            })
            .mount(&server)
            .await;

        let response = client(&server)
            .search(search_request(), &CancellationToken::new())
            .await
            .expect("third attempt should succeed");

        assert!(response.results.is_empty());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_deterministic_http_errors_and_redacts_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {"message": "invalid key test-secret"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let error = client(&server)
            .search(search_request(), &CancellationToken::new())
            .await
            .expect_err("unauthorized must fail");

        assert_eq!(error.category(), ErrorCategory::Upstream);
        let message = error.model_message();
        assert!(message.contains("HTTP 401"));
        assert!(!message.contains("test-secret"));
        assert!(message.contains("invalid key"));
    }

    #[tokio::test]
    async fn unsafe_upstream_messages_are_replaced_with_safe_summary() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"message": "failed at https://user:pass@example.com with secret other-secret"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let error = client(&server)
            .search(search_request(), &CancellationToken::new())
            .await
            .expect_err("unsafe upstream message must fail");

        let message = error.model_message();
        assert_eq!(
            message,
            "upstream service returned HTTP 400: upstream rejected the request"
        );
        assert!(!message.contains("https://user:pass@example.com"));
        assert!(!message.contains("other-secret"));
    }

    #[tokio::test]
    async fn rejects_response_body_larger_than_eight_mib() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 8 * 1024 * 1024 + 1]),
            )
            .mount(&server)
            .await;

        let error = client(&server)
            .search(search_request(), &CancellationToken::new())
            .await
            .expect_err("oversized response must fail");

        assert_eq!(error.category(), ErrorCategory::Upstream);
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_hanging_request() {
        let server = MockServer::start().await;
        let request_started = Arc::new(tokio::sync::Notify::new());
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(NotifyingResponder {
                request_started: Arc::clone(&request_started),
            })
            .mount(&server)
            .await;

        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            client(&server)
                .search(search_request(), &task_cancellation)
                .await
        });
        request_started.notified().await;
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("cancellation should be prompt")
            .expect("request task should not panic")
            .expect_err("cancelled request must fail");
        assert_eq!(result.category(), ErrorCategory::Cancelled);
        assert_eq!(result.model_message(), "request cancelled");
    }

    struct NotifyingResponder {
        request_started: Arc<tokio::sync::Notify>,
    }

    impl Respond for NotifyingResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            self.request_started.notify_one();
            ResponseTemplate::new(200).set_delay(Duration::from_secs(30))
        }
    }

    struct CountingResponder {
        attempts: Arc<AtomicUsize>,
    }

    impl Respond for CountingResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            if self.attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"results": []}))
            }
        }
    }
}
