//! One error type for every grabcli command.
//!
//! Each error carries a stable machine-readable [`ErrorKind`] so scripts and
//! assistants can react to it (`--json` prints `{"error":{"kind","message"}}`)
//! and a process exit code derived from the kind.

use serde::Serialize;

/// Stable error categories. The `snake_case` names are part of the CLI's
/// public interface: don't rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Bad arguments or input files.
    InvalidInput,
    /// No session saved yet: run `grab auth import`.
    AuthMissing,
    /// Grab rejected the session cookies (401) or they expired.
    AuthExpired,
    /// The hydra device JWT is missing, expired or rejected and could not be
    /// re-minted.
    HydraExpired,
    /// The Chrome DevTools endpoint could not be reached or misbehaved.
    CdpUnavailable,
    /// A request URL was not on the allowlist.
    UrlNotAllowed,
    /// Ordering is disabled in config (`allow_ordering = false`).
    OrderingDisabled,
    /// An order needs an interactive confirmation or `--yes <quote-id>`.
    ConfirmationRequired,
    /// The fresh quote differs from the one the user confirmed.
    QuoteChanged,
    /// The local cart is empty or invalid against the menu.
    CartInvalid,
    /// No saved delivery address (`location` cookie) in the session.
    AddressMissing,
    /// Something requested does not exist (merchant, item, order, voucher).
    NotFound,
    /// Grab answered 429.
    RateLimited,
    /// `grab auth login --otp`: the code was wrong or already used.
    OtpInvalid,
    /// The login page demanded an interactive captcha a headless tab can't do.
    CaptchaRequired,
    /// The pending `grab auth login` expired (or none was started).
    LoginExpired,
    /// Grab answered with an error we don't map more precisely.
    Api,
    /// Network / TLS / timeout.
    Network,
    /// Local file system error.
    Io,
    /// A response or file didn't have the expected shape.
    Parse,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::AuthMissing => "auth_missing",
            Self::AuthExpired => "auth_expired",
            Self::HydraExpired => "hydra_expired",
            Self::CdpUnavailable => "cdp_unavailable",
            Self::UrlNotAllowed => "url_not_allowed",
            Self::OrderingDisabled => "ordering_disabled",
            Self::ConfirmationRequired => "confirmation_required",
            Self::QuoteChanged => "quote_changed",
            Self::CartInvalid => "cart_invalid",
            Self::AddressMissing => "address_missing",
            Self::NotFound => "not_found",
            Self::RateLimited => "rate_limited",
            Self::OtpInvalid => "otp_invalid",
            Self::CaptchaRequired => "captcha_required",
            Self::LoginExpired => "login_expired",
            Self::Api => "api",
            Self::Network => "network",
            Self::Io => "io",
            Self::Parse => "parse",
        }
    }

    /// Process exit code for this kind. 0 is success, 1 generic failure.
    pub fn exit_code(self) -> i32 {
        match self {
            Self::InvalidInput | Self::CartInvalid | Self::NotFound | Self::AddressMissing => 2,
            Self::QuoteChanged => 3,
            Self::AuthMissing
            | Self::AuthExpired
            | Self::HydraExpired
            | Self::CdpUnavailable
            | Self::OtpInvalid
            | Self::CaptchaRequired
            | Self::LoginExpired => 4,
            Self::OrderingDisabled | Self::UrlNotAllowed => 5,
            Self::ConfirmationRequired => 6,
            Self::RateLimited => 7,
            Self::Api | Self::Network | Self::Io | Self::Parse => 1,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    /// Optional structured details (e.g. a quote diff), printed under
    /// `error.details` in `--json` mode.
    pub details: Option<serde_json::Value>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: None,
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Parse, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::NotFound, message)
    }

    pub fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }

    /// The `{"error": {...}}` object printed in `--json` mode.
    pub fn to_json(&self) -> serde_json::Value {
        let mut e = serde_json::json!({
            "kind": self.kind.as_str(),
            "message": self.message,
        });
        if let Some(d) = &self.details {
            e["details"] = d.clone();
        }
        serde_json::json!({ "error": e })
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::Io, e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ErrorKind::Parse, format!("JSON: {e}"))
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        // Only the local cache database uses SQLite.
        Self::new(ErrorKind::Io, format!("cache database: {e}"))
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        // reqwest errors can include the URL; ours never carry secrets in the
        // URL (cookies and tokens travel in headers), so this is safe to show.
        Self::new(ErrorKind::Network, e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_shape() {
        let e = Error::new(ErrorKind::HydraExpired, "hydra gone");
        let j = e.to_json();
        assert_eq!(j["error"]["kind"], "hydra_expired");
        assert_eq!(j["error"]["message"], "hydra gone");
        assert_eq!(e.exit_code(), 4);
    }

    #[test]
    fn address_missing_is_not_an_auth_error() {
        assert_eq!(ErrorKind::AddressMissing.as_str(), "address_missing");
        assert_eq!(ErrorKind::AddressMissing.exit_code(), 2);
    }

    #[test]
    fn quote_changed_exits_3() {
        assert_eq!(ErrorKind::QuoteChanged.exit_code(), 3);
    }
}
