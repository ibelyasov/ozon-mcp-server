use std::fmt;

#[derive(Debug)]
pub enum BrowserError {
    Cancelled,
    CommandFailed { command: String },
    CommandTimeout,
    DriverFailure { operation: &'static str },
    CleanupFailed,
    SessionPoisoned,
    InvalidBridgeResponse,
    ResponseTooLarge,
    CaptchaOrBlocked,
    InvalidOrigin,
    HttpStatus(u64),
    NavigationStatusUnavailable,
    UnexpectedRedirect,
}

impl BrowserError {
    pub fn should_retry(&self) -> bool {
        matches!(self, Self::CommandFailed { .. })
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => write!(formatter, "Request cancelled"),
            Self::CommandFailed { command } => {
                write!(
                    formatter,
                    "BROWSER_COMMAND_FAILED: agent-browser {command} failed"
                )
            }
            Self::CommandTimeout => {
                write!(
                    formatter,
                    "BROWSER_TIMEOUT: agent-browser command timed out"
                )
            }
            Self::DriverFailure { operation } => write!(
                formatter,
                "BROWSER_DRIVER_FAILED: agent-browser {operation} failed"
            ),
            Self::CleanupFailed => write!(
                formatter,
                "BROWSER_CLEANUP_FAILED: could not confirm private browser shutdown"
            ),
            Self::SessionPoisoned => write!(
                formatter,
                "BROWSER_CLEANUP_FAILED: previous session could not be closed; restart after closing its browser"
            ),
            Self::InvalidBridgeResponse => {
                write!(formatter, "INVALID_RESPONSE: invalid browser page outcome")
            }
            Self::ResponseTooLarge => {
                write!(formatter, "RESPONSE_TOO_LARGE: Ozon response exceeds 4 MiB")
            }
            Self::CaptchaOrBlocked => write!(
                formatter,
                "CAPTCHA_OR_BLOCKED: Ozon did not provide public product data"
            ),
            Self::InvalidOrigin => {
                write!(
                    formatter,
                    "SOURCE_CHANGED: Ozon page has an unexpected origin"
                )
            }
            Self::HttpStatus(status) => write!(formatter, "Ozon returned HTTP {status}"),
            Self::NavigationStatusUnavailable => {
                write!(formatter, "Ozon navigation HTTP status is unavailable")
            }
            Self::UnexpectedRedirect => write!(
                formatter,
                "Ozon redirected to a different page; requested data is unavailable"
            ),
        }
    }
}

impl std::error::Error for BrowserError {}

pub fn requires_reset(error: &anyhow::Error) -> bool {
    error.downcast_ref::<BrowserError>().is_some_and(|error| {
        matches!(
            error,
            BrowserError::CommandFailed { .. }
                | BrowserError::CommandTimeout
                | BrowserError::DriverFailure { .. }
                | BrowserError::CleanupFailed
                | BrowserError::SessionPoisoned
                | BrowserError::InvalidBridgeResponse
                | BrowserError::Cancelled
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_is_narrower_than_reset() {
        let command = BrowserError::CommandFailed {
            command: "eval".to_owned(),
        };
        assert!(command.should_retry());
        assert!(requires_reset(&anyhow::Error::new(command)));

        let timeout = BrowserError::CommandTimeout;
        assert!(!timeout.should_retry());
        assert!(requires_reset(&anyhow::Error::new(timeout)));

        let cancelled = BrowserError::Cancelled;
        assert!(!cancelled.should_retry());
        assert!(requires_reset(&anyhow::Error::new(cancelled)));

        assert!(!requires_reset(&anyhow::anyhow!("ordinary parse error")));
    }
}
