use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCategory {
    Configuration,
    Transport,
    Cancelled,
    Upstream,
    Tool,
    Mcp,
}

#[derive(Clone)]
pub enum UpstreamError {
    HttpStatus { status: u16, message: String },
    HttpStatusSummary { status: u16, summary: String },
    Protocol { detail: String },
}

#[derive(Clone)]
pub enum AppError {
    Configuration(String),
    Transport { detail: String },
    Cancelled,
    Upstream(UpstreamError),
    Tool { detail: String },
    Mcp { detail: String },
}

impl fmt::Debug for UpstreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HttpStatus { status, .. } => formatter
                .debug_struct("HttpStatus")
                .field("status", status)
                .field("message", &"<redacted>")
                .finish(),
            Self::HttpStatusSummary { status, .. } => formatter
                .debug_struct("HttpStatusSummary")
                .field("status", status)
                .field("summary", &"<redacted>")
                .finish(),
            Self::Protocol { .. } => formatter
                .debug_struct("Protocol")
                .field("detail", &"<redacted>")
                .finish(),
        }
    }
}

impl fmt::Debug for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppError")
            .field("category", &self.category())
            .finish()
    }
}

impl AppError {
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Configuration(_) => ErrorCategory::Configuration,
            Self::Transport { .. } => ErrorCategory::Transport,
            Self::Cancelled => ErrorCategory::Cancelled,
            Self::Upstream(_) => ErrorCategory::Upstream,
            Self::Tool { .. } => ErrorCategory::Tool,
            Self::Mcp { .. } => ErrorCategory::Mcp,
        }
    }

    pub fn model_message(&self) -> String {
        match self {
            Self::Configuration(_) => {
                "configuration error: set a non-blank DEEPSEEK_API_KEY and restart".to_owned()
            }
            Self::Transport { .. } => {
                "network error: could not reach the upstream service; retry later".to_owned()
            }
            Self::Cancelled => "request cancelled".to_owned(),
            Self::Upstream(UpstreamError::HttpStatus { status, .. }) => {
                format!("upstream service returned HTTP {status}; retry later")
            }
            Self::Upstream(UpstreamError::HttpStatusSummary { status, summary }) => {
                format!("upstream service returned HTTP {status}: {summary}")
            }
            Self::Upstream(UpstreamError::Protocol { .. }) => {
                "upstream protocol error: the service returned an unusable response".to_owned()
            }
            Self::Tool { .. } => "input error: provide valid tool arguments".to_owned(),
            Self::Mcp { .. } => "protocol error: the MCP request was invalid".to_owned(),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.model_message())
    }
}

impl std::error::Error for AppError {}

#[cfg(test)]
mod tests {
    use super::{AppError, ErrorCategory, UpstreamError};

    #[test]
    fn model_message_never_includes_secret_or_authorization() {
        let secret = "sk-test-secret";
        let errors = [
            AppError::Configuration("DEEPSEEK_API_KEY is missing".to_owned()),
            AppError::Upstream(UpstreamError::HttpStatus {
                status: 401,
                message: format!("Authorization: Bearer {secret}"),
            }),
            AppError::Upstream(UpstreamError::Protocol {
                detail: format!("raw body contains {secret}"),
            }),
            AppError::Transport {
                detail: format!("request debug dump {secret}"),
            },
            AppError::Tool {
                detail: format!("tool input included {secret}"),
            },
            AppError::Mcp {
                detail: format!("MCP debug dump {secret}"),
            },
        ];

        for error in errors {
            let message = error.model_message();
            assert!(!message.contains(secret));
            assert!(!message.contains("Authorization"));
            assert!(!message.contains("Bearer"));
            assert!(!message.contains("debug dump"));
            assert!(!message.contains("raw body"));
        }
    }

    #[test]
    fn categories_have_stable_safe_messages() {
        let cases = [
            (
                AppError::Configuration("internal detail".to_owned()),
                ErrorCategory::Configuration,
                "configuration",
            ),
            (
                AppError::Transport {
                    detail: "internal detail".to_owned(),
                },
                ErrorCategory::Transport,
                "network",
            ),
            (
                AppError::Cancelled,
                ErrorCategory::Cancelled,
                "request cancelled",
            ),
            (
                AppError::Tool {
                    detail: "internal detail".to_owned(),
                },
                ErrorCategory::Tool,
                "input",
            ),
            (
                AppError::Mcp {
                    detail: "internal detail".to_owned(),
                },
                ErrorCategory::Mcp,
                "protocol",
            ),
        ];

        for (error, category, expected_message) in cases {
            assert_eq!(error.category(), category);
            assert!(error.model_message().contains(expected_message));
        }
    }

    #[test]
    fn upstream_status_is_safe_and_preserves_status_only() {
        let error = AppError::Upstream(UpstreamError::HttpStatus {
            status: 503,
            message: "full upstream body".to_owned(),
        });

        assert_eq!(error.category(), ErrorCategory::Upstream);
        assert_eq!(
            error.model_message(),
            "upstream service returned HTTP 503; retry later"
        );
    }
}
