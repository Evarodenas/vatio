//! One error type for the library, the CLI and the MCP server.
//!
//! Each variant has a stable machine `code` and a process exit code, so scripts and
//! agents can branch on the failure without parsing prose.

use schemars::JsonSchema;
use serde::Serialize;

use crate::text;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum Error {
    /// The request itself is wrong: bad date, bad duration, unsupported combination.
    #[error("{0}")]
    Usage(String),

    /// The request is valid but the operator has not published that data (yet).
    #[error("{message}")]
    NotAvailable {
        message: String,
        hint: Option<String>,
        /// Whether the data is expected to appear later.
        expected: bool,
    },

    /// The upstream service could not be reached or timed out.
    #[error("could not reach {host}: {message}")]
    Network { host: String, message: String },

    /// The upstream service answered with an error or with data we cannot interpret.
    #[error("{message}")]
    Upstream {
        status: Option<u16>,
        message: String,
    },

    /// The data provider's bot protection turned the request away before it reached
    /// the API. It does this to a share of perfectly valid requests, at random.
    #[error("Red Eléctrica's bot protection refused the request (HTTP {status})")]
    Blocked { status: u16 },

    /// A bug in this program.
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    pub fn usage(message: impl Into<String>) -> Self {
        Self::Usage(message.into())
    }

    pub fn internal(err: impl std::fmt::Display) -> Self {
        Self::Internal(err.to_string())
    }

    /// Data that is not out yet but will be: worth asking again later.
    pub fn not_yet(message: impl Into<String>, hint: Option<&str>) -> Self {
        Self::NotAvailable {
            message: message.into(),
            hint: hint.map(str::to_owned),
            expected: true,
        }
    }

    /// Data the operator does not have for that request, now or later.
    pub fn no_data(message: impl Into<String>) -> Self {
        Self::NotAvailable {
            message: message.into(),
            hint: None,
            expected: false,
        }
    }

    /// Stable identifier for programmatic handling. Part of the output contract.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Usage(_) => ErrorCode::Usage,
            Self::NotAvailable { .. } => ErrorCode::NotAvailable,
            Self::Network { .. } => ErrorCode::Network,
            Self::Upstream { .. } => ErrorCode::Upstream,
            Self::Blocked { .. } => ErrorCode::Blocked,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    /// The message, with anything unprintable that upstream text may have carried removed.
    pub fn message(&self) -> String {
        text::clean(&self.to_string())
    }

    /// Process exit code. Part of the output contract.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Internal(_) => 1,
            Self::Usage(_) => 2,
            Self::NotAvailable { .. } => 3,
            Self::Network { .. } | Self::Upstream { .. } | Self::Blocked { .. } => 4,
        }
    }

    /// Whether trying again later can succeed without changing the request.
    pub fn retryable(&self) -> bool {
        match self {
            Self::NotAvailable { expected, .. } => *expected,
            Self::Network { .. } | Self::Blocked { .. } => true,
            // Server-side trouble and rate limiting pass; a response that cannot be
            // read, or a client error, will not get better by asking again.
            Self::Upstream { status, .. } => status.is_none_or(|s| s >= 500 || s == 429),
            Self::Usage(_) | Self::Internal(_) => false,
        }
    }

    pub fn hint(&self) -> Option<&str> {
        match self {
            Self::NotAvailable { hint, .. } => hint.as_deref(),
            Self::Network { .. } => Some("check the connection and retry"),
            Self::Blocked { .. } => Some(
                "nothing is wrong with the request; retry in a few seconds. Days already cached keep working",
            ),
            Self::Upstream {
                status: Some(429), ..
            } => Some("the data service is rate limiting; wait a minute before retrying"),
            _ => None,
        }
    }

    pub fn report(&self) -> ErrorReport {
        ErrorReport {
            error: ErrorBody {
                code: self.code(),
                message: self.message(),
                hint: self.hint().map(str::to_owned),
                retryable: self.retryable(),
            },
        }
    }
}

/// What kind of failure it was. Part of the output contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request is wrong; fix the arguments.
    Usage,
    /// The data is not published, yet or at all.
    NotAvailable,
    /// The data service could not be reached.
    Network,
    /// The data service answered with an error or something unreadable.
    Upstream,
    /// The data service's bot protection refused a valid request.
    Blocked,
    /// A bug in this program.
    Internal,
}

/// The JSON error object written to stderr under `--json`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ErrorReport {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    /// What to do about it, when there is something specific to say.
    pub hint: Option<String>,
    /// True when the same request may succeed later.
    pub retryable: bool,
}
