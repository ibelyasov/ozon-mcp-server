//! Typed errors at the local runtime boundary. Domain failures retain their own DTO.
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    InvalidArgument,
    InvalidConfiguration,
    InvalidReference,
    ContextChanged,
    ContextUnverified,
    ResearchExpired,
    UnsupportedCapability,
    SourceBlocked,
    PartialResult,
    NotFound,
    Conflict,
    StorageFull,
    ServerBusy,
    SourceChanged,
    Cancelled,
    UpstreamTimeout,
    ResultTooLarge,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::InvalidConfiguration => "INVALID_ARGUMENT",
            Self::InvalidReference => "INVALID_REFERENCE",
            Self::ContextChanged => "CONTEXT_CHANGED",
            Self::ContextUnverified => "CONTEXT_UNVERIFIED",
            Self::ResearchExpired => "RESEARCH_EXPIRED",
            Self::UnsupportedCapability => "UNSUPPORTED_CAPABILITY",
            Self::SourceBlocked => "SOURCE_BLOCKED",
            Self::PartialResult => "PARTIAL_RESULT",
            Self::NotFound => "NOT_FOUND",
            Self::Conflict => "CONFLICT",
            Self::StorageFull => "STORAGE_FULL",
            Self::ServerBusy => "SERVER_BUSY",
            Self::SourceChanged => "SOURCE_CHANGED",
            Self::Cancelled => "CANCELLED",
            Self::UpstreamTimeout => "UPSTREAM_TIMEOUT",
            Self::ResultTooLarge => "RESULT_TOO_LARGE",
        }
    }
}

#[derive(Debug)]
pub struct RuntimeError {
    pub code: Code,
    message: String,
}

impl RuntimeError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::new(Code::InvalidConfiguration, message)
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for RuntimeError {}

pub fn code(error: &anyhow::Error) -> &'static str {
    if let Some(error) = error.downcast_ref::<RuntimeError>() {
        return error.code.as_str();
    }
    if let Some(error) = error.downcast_ref::<crate::runtime::browser::error::BrowserError>() {
        return error.code();
    }
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| matches!(io.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT)))
    }) {
        return "STORAGE_FULL";
    }
    if error.chain().any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some_and(|sql| matches!(sql, rusqlite::Error::SqliteFailure(detail, _) if detail.code == rusqlite::ErrorCode::DiskFull))) { return "STORAGE_FULL"; }
    "SOURCE_CHANGED"
}

pub fn fail(code: Code, message: impl Into<String>) -> anyhow::Error {
    RuntimeError::new(code, message).into()
}

pub fn safe_message(error: &anyhow::Error) -> String {
    if let Some(error) = error.downcast_ref::<RuntimeError>() {
        return error
            .to_string()
            .chars()
            .filter(|c| !c.is_control())
            .take(1500)
            .collect();
    }
    if let Some(error) = error.downcast_ref::<crate::runtime::browser::error::BrowserError>() {
        return error
            .to_string()
            .chars()
            .filter(|c| !c.is_control())
            .take(1500)
            .collect();
    }
    "The local runtime could not complete this operation".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrapped_typed_and_disk_full_causes_keep_their_codes() {
        let error =
            fail(Code::ContextChanged, "Context changed").context("private internal details");
        assert_eq!(code(&error), "CONTEXT_CHANGED");
        assert_eq!(safe_message(&error), "Context changed");
        let io = anyhow::Error::new(std::io::Error::from_raw_os_error(libc::ENOSPC))
            .context("journal path");
        assert_eq!(code(&io), "STORAGE_FULL");
        let sql = anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            None,
        ))
        .context("journal path");
        assert_eq!(code(&sql), "STORAGE_FULL");
        assert_eq!(
            safe_message(&sql),
            "The local runtime could not complete this operation"
        );
        assert_eq!(
            code(&anyhow::anyhow!("CANCELLED: untrusted source text")),
            "SOURCE_CHANGED"
        );
    }
}
