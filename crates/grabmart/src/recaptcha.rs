//! reCAPTCHA v3 tokens for the GrabMart `public` API.
//!
//! Every public mart call needs an `X-Recaptcha-Token` header; without it Grab
//! answers `403 {"message":"Recaptcha failed"}`. The token is minted by
//! Google's script with `grecaptcha.execute(siteKey, {action})`, where `action`
//! is the endpoint name (`searchMerchant`, `getMerchant`, ...). Tokens are
//! single-use and expire after ~2 minutes, and the site key only works on
//! `mart.grab.com`, so a plain HTTP client cannot produce one.
//!
//! grabcli mints them in the user's CDP browser (config `cdp`, default
//! `http://127.0.0.1:9222`): it evaluates `grecaptcha.execute` in a
//! `mart.grab.com` tab (opening one in the background if needed) and sends the
//! token with the request from Rust. No login is needed for this. Tokens are
//! never printed, logged or cached.

use std::time::Duration;

use grabcli_core::cdp;
use grabcli_core::error::{Error, ErrorKind, Result};

use crate::api;

/// The public site key from mart.grab.com's page config.
pub const SITE_KEY: &str = "6LfnMFIfAAAAAGCE6O5Kscpsg0ATxuLrmbxtnXCc";

/// reCAPTCHA actions (= the web app's endpoint names).
pub mod action {
    pub const FEED: &str = "getFeed";
    pub const CATEGORY: &str = "getCategory";
    pub const SEARCH_MERCHANT: &str = "searchMerchant";
    pub const MERCHANT: &str = "getMerchant";
    pub const MERCHANT_ITEMS: &str = "getMerchantItems";
    pub const SEARCH_IN_STORE: &str = "searchInStore";
    pub const ITEM_DETAIL: &str = "getItemDetail";
}

/// The JS evaluated in the mart.grab.com tab. Loads Google's script when the
/// page hasn't yet, then returns a fresh token for `action`.
pub fn mint_expression(action: &str) -> Result<String> {
    if action.is_empty() || !action.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(Error::invalid(format!("bad reCAPTCHA action `{action}`")));
    }
    Ok(format!(
        r#"(async () => {{
  const key = "{SITE_KEY}";
  if (typeof grecaptcha === "undefined" || !grecaptcha.execute) {{
    await new Promise((resolve, reject) => {{
      const s = document.createElement("script");
      s.src = "https://www.google.com/recaptcha/api.js?render=" + key;
      s.onload = resolve;
      s.onerror = () => reject(new Error("reCAPTCHA script failed to load"));
      document.head.appendChild(s);
    }});
  }}
  await new Promise((r) => grecaptcha.ready(r));
  return await grecaptcha.execute(key, {{ action: "{action}" }});
}})()"#
    ))
}

/// Mint one token for `action` via the CDP browser at `cdp_base`.
pub async fn mint(cdp_base: &str, action: &str) -> Result<String> {
    let expr = mint_expression(action)?;
    let v = cdp::evaluate_in_origin(
        cdp_base,
        api::ORIGIN,
        api::RECAPTCHA_PAGE,
        &expr,
        Duration::from_secs(30),
    )
    .await
    .map_err(|e| {
        Error::new(
            e.kind,
            format!(
                "GrabMart needs a reCAPTCHA token minted in a browser over CDP ({cdp_base}): {}. \
                 Start the browser with remote debugging (config `cdp`).",
                e.message
            ),
        )
    })?;
    match v.as_str() {
        Some(t) if t.len() > 20 => Ok(t.to_string()),
        _ => Err(Error::new(
            ErrorKind::CdpUnavailable,
            "the browser returned no reCAPTCHA token",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_embeds_action_and_key() -> Result<()> {
        let e = mint_expression(action::SEARCH_IN_STORE)?;
        assert!(e.contains(r#"action: "searchInStore""#));
        assert!(e.contains(SITE_KEY));
        Ok(())
    }

    #[test]
    fn rejects_injection_in_action() {
        assert!(mint_expression(r#"x"});alert(1);//"#).is_err());
        assert!(mint_expression("").is_err());
    }
}
