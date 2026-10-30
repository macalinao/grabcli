//! Minimal JWT helpers: decode the (unverified) payload, read `exp`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

use crate::error::{Error, Result};

pub fn b64url(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

fn b64url_decode(s: &str) -> Result<Vec<u8>> {
    URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .map_err(|e| Error::parse(format!("bad base64url in JWT: {e}")))
}

/// Decode a JWT's payload without verifying the signature.
pub fn payload(token: &str) -> Result<Value> {
    let part = token
        .split('.')
        .nth(1)
        .ok_or_else(|| Error::parse("not a JWT"))?;
    Ok(serde_json::from_slice(&b64url_decode(part)?)?)
}

/// `exp` claim in Unix seconds, if present.
pub fn exp(token: &str) -> Option<i64> {
    payload(token).ok()?.get("exp")?.as_i64()
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_payload() -> Result<()> {
        let tok = format!(
            "{}.{}.sig",
            b64url(br#"{"alg":"none"}"#),
            b64url(br#"{"exp":1700000000,"sessionKey":"abc"}"#)
        );
        assert_eq!(exp(&tok), Some(1_700_000_000));
        assert_eq!(payload(&tok)?["sessionKey"], "abc");
        Ok(())
    }
}
