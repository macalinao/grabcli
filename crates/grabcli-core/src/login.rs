//! `grab auth login`: a real phone login on weblogin.grab.com, driven over
//! CDP inside an **isolated browser context**.
//!
//! grabcli does not replay or forge the login page's anti-bot tokens
//! (reCAPTCHA Enterprise, the `x-hydraweb-jwt` fingerprint). Instead it opens
//! `food.grab.com/auth/login` in a fresh `Target.createBrowserContext`
//! context (own cookie jar, shared with no other tab, so nothing else in the
//! browser can log it out) and lets the real page do its own checks, the way
//! a person would. grabcli only:
//!
//! 1. types the phone number and keeps "Remember me … 60 days" ticked;
//! 2. clicks Continue and, if Grab offers a choice, picks ONE challenge
//!    (in-app approval or OTP) — this is the only step that sends anything;
//! 3. later (same or another process) types the OTP, or waits for the
//!    in-app approval;
//! 4. lets the page redirect through `food.grab.com/auth/callback` inside the
//!    context, reads the context's cookies with `Storage.getCookies`, saves
//!    them as the session and disposes the context.
//!
//! The context outlives the process (`disposeOnDetach: false`); what's needed
//! to resume (context/tab ids, expiry, the masked phone) is the
//! [`PendingLogin`] value, which the caller keeps (the CLI in a 0600 file, a
//! daemon in memory). The step functions take it as `&mut Option<_>` and set
//! it to `None` once the login is over (logged in, or ended for good). Sends
//! are rate-limited by an injectable [`SendBudget`] and never retried.
//!
//! This module writes no files and logs nothing: it never prints cookie
//! values, tokens, OTPs or the full phone number. [`LoginDriver`] wraps the
//! steps as a small state machine for daemons (one driver per login, driven
//! from a thread via `block_on` or from a task).

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cdp::{self, Browser, PageInfo};
use crate::error::{Error, ErrorKind, Result};
use crate::jwt;
use crate::session::{LOCATION_COOKIE, Session};
use crate::sso;

/// Where the browser login starts (redirects to weblogin.grab.com).
pub const LOGIN_START_URL: &str = sso::FOOD_LOGIN;
/// How long a started login stays resumable. Grab's codes live ~5 min.
pub const PENDING_TTL_SECS: i64 = 10 * 60;
/// Minimum gap between two challenge sends.
pub const SEND_COOLDOWN_SECS: i64 = 60;
/// Maximum challenge sends in any rolling hour.
pub const MAX_SENDS_PER_HOUR: usize = 3;

// ------------------------------------------------------------------- phone

/// Calling codes of the markets Grab serves, longest first.
const CALLING_CODES: &[&str] = &["855", "60", "62", "63", "65", "66", "84", "95"];

/// Split `+<cc><national>` into (`cc`, national digits). Spaces, dashes,
/// dots and parentheses are ignored; a trunk `0` is dropped.
pub fn split_phone(phone: &str) -> Result<(String, String)> {
    let cleaned: String = phone
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '.' | '(' | ')'))
        .collect();
    let digits = cleaned.strip_prefix('+').ok_or_else(|| {
        Error::invalid("phone must be in international form, e.g. +63 9XX XXX XXXX")
    })?;
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(Error::invalid("phone must contain only digits after the +"));
    }
    let cc = CALLING_CODES
        .iter()
        .find(|cc| digits.starts_with(**cc))
        .ok_or_else(|| Error::invalid("phone country code is not a Grab market"))?;
    let national = digits[cc.len()..].trim_start_matches('0').to_string();
    if !(6..=12).contains(&national.len()) {
        return Err(Error::invalid("phone number has the wrong length"));
    }
    Ok(((*cc).to_string(), national))
}

/// `+63••••••••12`: the country code and the last two digits only.
pub fn mask_phone(cc: &str, national: &str) -> String {
    let n = national.len();
    let keep = n.min(2);
    format!("+{cc}{}{}", "•".repeat(n - keep), &national[n - keep..])
}

// ------------------------------------------------------------------ channel

/// Which challenge to ask Grab for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Approve in the Grab app (push).
    App,
    /// A one-time code by SMS.
    Sms,
    /// A one-time code by WhatsApp.
    Whatsapp,
}

impl Channel {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "app" | "push" => Ok(Self::App),
            "sms" => Ok(Self::Sms),
            "whatsapp" | "wa" => Ok(Self::Whatsapp),
            _ => Err(Error::invalid("--channel must be app, sms or whatsapp")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Sms => "sms",
            Self::Whatsapp => "whatsapp",
        }
    }

    /// What one send produces, in words.
    pub fn describe(self) -> &'static str {
        match self {
            Self::App => "one approval request in the Grab app",
            Self::Sms | Self::Whatsapp => {
                "one OTP code (Grab chooses SMS or WhatsApp delivery; the CLI never resends to switch)"
            }
        }
    }

    /// weblogin challenge types (the `Proceed to <TYPE>` options) that
    /// satisfy this channel, best first.
    pub fn challenge_types(self) -> &'static [&'static str] {
        match self {
            Self::App => &["PUSH_V2", "PUSH"],
            Self::Sms | Self::Whatsapp => &["CHANNEL_OPTIMIZED_PHONE_OTP", "SMS_OTP"],
        }
    }
}

// ------------------------------------------------------------ pending state

/// What the send produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingKind {
    /// A code was sent (SMS/WhatsApp); finish with `--otp`.
    OtpSent,
    /// An in-app approval was requested; finish with `--poll`.
    ApprovalPending,
}

impl PendingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OtpSent => "otp_sent",
            Self::ApprovalPending => "approval_pending",
        }
    }
}

/// State kept between `--phone …` and `--otp` / `--poll`. No secrets:
/// ids of the isolated context/tab in the browser and a masked phone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingLogin {
    pub kind: PendingKind,
    pub cdp_base: String,
    pub browser_context_id: String,
    pub target_id: String,
    pub created_at: i64,
    pub expires_at: i64,
    pub phone_masked: String,
    pub channel_requested: Channel,
    /// Delivery Grab reported on the page (`SMS`, `WhatsApp`, `app`), if known.
    #[serde(default)]
    pub channel_sent: Option<String>,
    /// Whether to copy the saved address (`location` cookie) from the
    /// browser's default context when the session has none. The CLI does
    /// (it's the user's own browser); a shared daemon browser must not.
    #[serde(default = "yes")]
    pub copy_browser_address: bool,
}

fn yes() -> bool {
    true
}

impl PendingLogin {
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at <= now
    }

    /// The `--json` document for a pending login: `otp_sent` or
    /// `approval_pending`, with the command that finishes it.
    pub fn next_command(&self) -> &'static str {
        match self.kind {
            PendingKind::OtpSent => "grab auth login --otp <code>",
            PendingKind::ApprovalPending => "grab auth login --poll",
        }
    }

    pub fn summary(&self, now: i64) -> Value {
        json!({
            "kind": self.kind.as_str(),
            "phone": self.phone_masked,
            "channel_requested": self.channel_requested.as_str(),
            "channel_sent": self.channel_sent,
            "expires_in_secs": (self.expires_at - now).max(0),
            "expired": self.is_expired(now),
        })
    }
}

/// stdout document after `--phone` (or a `--poll` that is still waiting).
pub fn pending_json(p: &PendingLogin, now: i64) -> Value {
    let mut v = p.summary(now);
    v["expires_at"] = json!(p.expires_at);
    v["next"] = json!(p.next_command());
    v
}

// ---------------------------------------------------------------- send log

/// Timestamps of challenge sends, kept separately from the pending login so
/// cancelling a login never resets the limits. A plain value: persisting it
/// is up to the [`SendBudget`] that holds it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SendLog {
    pub sends: Vec<i64>,
}

impl SendLog {
    fn recent(&self, now: i64) -> impl Iterator<Item = i64> + '_ {
        self.sends.iter().copied().filter(move |t| now - t < 3600)
    }

    pub fn sends_last_hour(&self, now: i64) -> usize {
        self.recent(now).count()
    }

    /// Seconds until another send is allowed (0 = now).
    pub fn wait_secs(&self, now: i64) -> i64 {
        let mut recent: Vec<i64> = self.recent(now).collect();
        recent.sort_unstable();
        let cooldown = recent
            .last()
            .map_or(0, |last| last + SEND_COOLDOWN_SECS - now);
        let hourly = if recent.len() >= MAX_SENDS_PER_HOUR {
            recent[recent.len() - MAX_SENDS_PER_HOUR] + 3600 - now
        } else {
            0
        };
        cooldown.max(hourly).max(0)
    }

    /// Fail with `rate_limited` if a send now would break the cooldown or
    /// the hourly cap.
    pub fn check(&self, now: i64) -> Result<()> {
        let wait = self.wait_secs(now);
        if wait == 0 {
            return Ok(());
        }
        let why = if self.sends_last_hour(now) >= MAX_SENDS_PER_HOUR {
            format!("{MAX_SENDS_PER_HOUR} login challenges were already sent in the last hour")
        } else {
            format!("a login challenge was sent less than {SEND_COOLDOWN_SECS}s ago")
        };
        Err(Error::new(
            ErrorKind::RateLimited,
            format!("{why}; nothing was sent. Try again in {wait}s."),
        )
        .with_details(json!({"retry_after_secs": wait})))
    }

    /// Record a send (before it happens: a crash still counts) and drop
    /// entries older than an hour.
    pub fn record(&mut self, now: i64) {
        self.sends.retain(|t| now - t < 3600);
        self.sends.push(now);
    }
}

/// The challenge-send budget ([`SEND_COOLDOWN_SECS`],
/// [`MAX_SENDS_PER_HOUR`]). Injectable so the caller decides its scope and
/// storage: the CLI keeps one per user in a file; a daemon keeps one per org.
#[async_trait]
pub trait SendBudget: Send + Sync {
    /// Fail with `rate_limited` if a send now would break the limits.
    async fn check(&self, now: i64) -> Result<()>;
    /// Record a send. Called before the send happens (a crash still counts);
    /// an error aborts the login before anything is sent.
    async fn record(&self, now: i64) -> Result<()>;
}

/// A [`SendLog`] held in memory.
#[derive(Debug, Default)]
pub struct MemorySendBudget {
    pub log: Mutex<SendLog>,
}

impl MemorySendBudget {
    fn lock(&self) -> std::sync::MutexGuard<'_, SendLog> {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A copy of the log.
    pub fn snapshot(&self) -> SendLog {
        self.lock().clone()
    }
}

#[async_trait]
impl SendBudget for MemorySendBudget {
    async fn check(&self, now: i64) -> Result<()> {
        self.lock().check(now)
    }

    async fn record(&self, now: i64) -> Result<()> {
        self.lock().record(now);
        Ok(())
    }
}

// ------------------------------------------------------- shared-tab warning

/// food.grab.com tabs in the shared browser that poll order tracking. When
/// the browser's food session lapses, such a tab gets 401s from `/track` and
/// the page then calls GrabID `oauth2/logout` + food `/auth/logout` (every
/// ~16 s), which logs the browser out again right after any login there.
/// The isolated login context is immune, but these tabs still break
/// `grab auth import` and normal browsing. grabcli never closes them.
pub fn order_tracking_tabs(pages: &[PageInfo]) -> Vec<String> {
    pages
        .iter()
        .filter_map(|p| {
            let u = reqwest::Url::parse(&p.url).ok()?;
            let path = u.path();
            (u.host_str() == Some("food.grab.com")
                && (path.contains("/order/") || path.contains("/track")))
            .then(|| format!("food.grab.com{path}"))
        })
        .collect()
}

pub fn tracking_tab_warning(tabs: &[String]) -> Option<String> {
    if tabs.is_empty() {
        return None;
    }
    Some(format!(
        "{} food.grab.com order-tracking tab(s) may be open in the browser ({}). When the \
         browser's food session lapses such tabs poll /track, get 401 and call Grab's logout \
         every ~16s, which logs that browser out. If they are open, close them (grabcli won't \
         touch other tabs).",
        tabs.len(),
        tabs.join(", ")
    ))
}

// ------------------------------------------------------------- page model

/// Where the weblogin page is, from [`PROBE_JS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// Still loading / spinner.
    Loading,
    /// The phone-number form.
    Phone,
    /// "Choose a preferred way to sign in" with these challenge types.
    Options(Vec<String>),
    /// Waiting for a code; `channel` as shown ("SMS", "WhatsApp", …).
    Otp {
        channel: Option<String>,
    },
    /// The code was not accepted.
    OtpRejected,
    /// Waiting for an in-app approval.
    Approval,
    /// Back on food.grab.com: logged in.
    Done,
    /// A step-up that needs a human (reCAPTCHA v2, selfie, PIN, …).
    StepUp(String),
    /// An error banner (blocked, rate limited, expired, rejected, …).
    Problem(String),
    Unknown(String),
}

/// Read-only probe of the login tab. Returns structure and fixed UI strings
/// only; the CLI never prints `text`.
pub const PROBE_JS: &str = r#"(() => {
  const root = document.getElementById('root');
  const options = [...document.querySelectorAll('[role=button] img[alt^="Proceed to "]')]
    .map(i => i.getAttribute('alt').slice('Proceed to '.length));
  const captcha = [...document.querySelectorAll('iframe')].some(f =>
    /recaptcha\/(api2|enterprise)\/bframe/.test(f.src || '') && f.getBoundingClientRect().height > 100);
  return {
    host: location.host, path: location.pathname, ready: document.readyState,
    has_phone: !!document.getElementById('Phone'), options, captcha,
    text: ((root && root.innerText) || (document.body && document.body.innerText) || '').slice(0, 3000),
  };
})()"#;

const PROBLEMS: &[&str] = &[
    "Account temporarily blocked",
    "temporarily locked",
    "You've tried too many times",
    "Try again later",
    "Too many attempts",
    "Unable to resend code",
    "OTP challenge expired",
    "Request rejected",
    "QR Code Expired",
    "Account Not Found",
    "Oops, something went wrong",
];

pub fn classify_screen(p: &Value) -> Screen {
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default();
    let host = s("host");
    let path = s("path");
    let text = s("text");
    if host == "food.grab.com" && !path.starts_with("/auth/") {
        return Screen::Done;
    }
    if host != "weblogin.grab.com" {
        return if host.is_empty() || host == "food.grab.com" {
            Screen::Loading
        } else {
            Screen::Unknown(host.to_string())
        };
    }
    if p.get("captcha").and_then(Value::as_bool) == Some(true) {
        return Screen::StepUp("recaptcha".into());
    }
    if let Some(pr) = PROBLEMS.iter().find(|w| text.contains(**w)) {
        return Screen::Problem((*pr).to_string());
    }
    if text.contains("Incorrect code") || text.contains("no longer valid") {
        return Screen::OtpRejected;
    }
    let segs: Vec<&str> = path.split('/').filter(|x| !x.is_empty()).collect();
    let has = |x: &str| segs.contains(&x);
    let otp_channel = || {
        ["SMS", "WhatsApp", "Viber", "Zalo"]
            .iter()
            .find(|c| text.contains(&format!("Check your {c}")))
            .map(|c| (*c).to_string())
    };
    if has("optimised-sms-otp") || has("sms-otp") || has("email-otp") {
        return Screen::Otp {
            channel: otp_channel(),
        };
    }
    if has("push-v2") || has("push") {
        return Screen::Approval;
    }
    if let Some(seg) = [
        "recaptcha",
        "arkose",
        "web-selfie",
        "passkey",
        "password",
        "pin",
    ]
    .iter()
    .find(|x| has(x))
    {
        return Screen::StepUp((*seg).to_string());
    }
    let options: Vec<String> = p
        .get("options")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    if has("challenge-options") || (!options.is_empty() && text.contains("Choose")) {
        return Screen::Options(options);
    }
    // Fallbacks on the UI copy in case the route names change.
    if text.contains("Approve in mobile app") || text.contains("App push notification sent") {
        return Screen::Approval;
    }
    if text.contains("Check your") || text.contains("Enter verification code") {
        return Screen::Otp {
            channel: otp_channel(),
        };
    }
    if p.get("has_phone").and_then(Value::as_bool) == Some(true) {
        return Screen::Phone;
    }
    Screen::Loading
}

/// Pick the challenge option to click for `channel`, if Grab offers one.
pub fn pick_option(channel: Channel, offered: &[String]) -> Option<&'static str> {
    channel
        .challenge_types()
        .iter()
        .copied()
        .find(|t| offered.iter().any(|o| o == t))
}

// ------------------------------------------------------------- dry run

/// What `--phone …` would do, without doing it (`--dry-run`).
pub fn dry_run_plan(
    phone_masked: &str,
    channel: Channel,
    cdp_base: &str,
    log: &SendLog,
    now: i64,
) -> Value {
    json!({
        "kind": "dry_run",
        "dry_run": true,
        "sent": false,
        "phone": phone_masked,
        "channel": channel.as_str(),
        "cdp": cdp_base,
        "would_send": channel.describe(),
        "steps": [
            "try a silent SSO refresh first (never sends anything); stop if it works",
            "open food.grab.com/auth/login in a NEW isolated browser context (no shared cookies, other tabs untouched)",
            "type the phone number, keep \"Remember me on this device for 60 days\" ticked, click Continue",
            format!("if Grab offers a choice, pick {} (once; never resend or retry)", channel.as_str()),
            format!("stop and keep the login tab alive for {} min; finish with `grab auth login --otp <code>` or `grab auth login --poll`", PENDING_TTL_SECS / 60),
        ],
        "sends_last_hour": log.sends_last_hour(now),
        "max_sends_per_hour": MAX_SENDS_PER_HOUR,
        "next_send_allowed_in_secs": log.wait_secs(now),
    })
}

// ------------------------------------------------------------ the driver

/// JS: centre of the first element matching `selector` (scrolled into
/// view), or null.
fn rect_js(find_expr: &str) -> String {
    format!(
        r#"(() => {{ const el = {find_expr}; if (!el) return null;
  el.scrollIntoView({{block: 'center'}}); const r = el.getBoundingClientRect();
  if (r.width === 0 || r.height === 0) return null;
  return {{x: r.left + r.width / 2, y: r.top + r.height / 2, disabled: !!el.disabled}}; }})()"#
    )
}

const CONTINUE_BTN: &str = "[...document.querySelectorAll('button.dui-btn-primary, button')].find(b => b.innerText.trim() === 'Continue')";
const PHONE_INPUT: &str = "document.getElementById('Phone')";
const REMEMBER_LABEL: &str =
    "[...document.querySelectorAll('label')].find(l => /Remember me/.test(l.innerText))";
const REMEMBER_STATE: &str = r#"(() => { const l = [...document.querySelectorAll('label')].find(l => /Remember me/.test(l.innerText));
  if (!l) return 'absent'; const i = l.querySelector('input[type=checkbox]');
  return (i && i.checked) || /checkbox-wrapper-checked/.test(l.className) ? 'checked' : 'unchecked'; })()"#;
/// The OTP input: the first visible, empty-ish text-like input on the page.
const OTP_INPUT: &str = "[...document.querySelectorAll('#root input')].filter(i => i.id !== 'Phone' && !['checkbox','search','hidden'].includes(i.type) && i.offsetParent !== null).find(i => !i.value) || [...document.querySelectorAll('#root input')].find(i => i.id !== 'Phone' && !['checkbox','search','hidden'].includes(i.type) && i.offsetParent !== null)";

struct Tab {
    browser: Browser,
    session: String,
}

impl Tab {
    async fn probe(&mut self) -> Result<Screen> {
        // Keep the CDP event buffer from growing while we poll.
        self.browser.drain_events(Duration::ZERO).await.ok();
        match self.browser.eval(&self.session, PROBE_JS).await {
            Ok(v) => Ok(classify_screen(&v)),
            // Mid-navigation the execution context can vanish; treat as loading.
            Err(_) => Ok(Screen::Loading),
        }
    }

    /// Poll until `done(screen)` or `timeout`; returns the last screen.
    async fn wait_for(
        &mut self,
        timeout: Duration,
        done: impl Fn(&Screen) -> bool,
    ) -> Result<Screen> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let s = self.probe().await?;
            if done(&s) || tokio::time::Instant::now() >= deadline {
                return Ok(s);
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    }

    async fn rect(&mut self, find_expr: &str) -> Result<Option<(f64, f64, bool)>> {
        let v = self
            .browser
            .eval(&self.session, &rect_js(find_expr))
            .await?;
        Ok(
            match (
                v.get("x").and_then(Value::as_f64),
                v.get("y").and_then(Value::as_f64),
            ) {
                (Some(x), Some(y)) => Some((
                    x,
                    y,
                    v.get("disabled").and_then(Value::as_bool).unwrap_or(false),
                )),
                _ => None,
            },
        )
    }

    async fn click(&mut self, find_expr: &str, what: &str) -> Result<()> {
        let (x, y, _) = self
            .rect(find_expr)
            .await?
            .ok_or_else(|| Error::new(ErrorKind::Api, format!("login page: no {what} found")))?;
        self.browser.click_at(&self.session, x, y).await
    }
}

/// Options for [`start`].
#[derive(Clone, Copy)]
pub struct StartOpts<'a> {
    pub cdp_base: &'a str,
    pub phone: &'a str,
    pub channel: Channel,
    /// See [`PendingLogin::copy_browser_address`].
    pub copy_browser_address: bool,
}

fn page_error(s: &Screen) -> Error {
    match s {
        Screen::StepUp(kind) => Error::new(
            ErrorKind::CaptchaRequired,
            format!(
                "Grab asked for an extra check ({kind}) that needs a person. Log in at food.grab.com \
                 in a normal browser window instead, then run `grab auth import`"
            ),
        ),
        Screen::Problem(p) if p.contains("block") || p.contains("many") || p.contains("later") => {
            Error::new(
                ErrorKind::RateLimited,
                format!("Grab says: {p}. Wait before trying again."),
            )
        }
        Screen::Problem(p) => Error::new(ErrorKind::LoginExpired, format!("Grab says: {p}")),
        other => Error::new(
            ErrorKind::Api,
            format!(
                "the login page is in an unexpected state ({})",
                screen_name(other)
            ),
        ),
    }
}

fn screen_name(s: &Screen) -> &'static str {
    match s {
        Screen::Loading => "loading",
        Screen::Phone => "phone form",
        Screen::Options(_) => "challenge options",
        Screen::Otp { .. } => "code entry",
        Screen::OtpRejected => "code rejected",
        Screen::Approval => "in-app approval",
        Screen::Done => "logged in",
        Screen::StepUp(_) => "step-up",
        Screen::Problem(_) => "error banner",
        Screen::Unknown(_) => "unknown",
    }
}

/// Best-effort: dispose the isolated context (and its tab and cookies).
async fn dispose(cdp_base: &str, context: &str) {
    if let Ok(mut b) = Browser::connect(cdp_base).await {
        b.dispose_context(context).await.ok();
    }
}

/// Start a login and send exactly ONE challenge. The caller has already
/// decided a send is wanted, that silent refresh failed, and that no other
/// login is pending (see [`check_no_pending`]). Returns the pending login to
/// keep for [`finish_with_otp`] / [`wait_for_approval`].
pub async fn start(opts: StartOpts<'_>, budget: &dyn SendBudget) -> Result<PendingLogin> {
    let now = jwt::now();
    let (cc, national) = split_phone(opts.phone)?;
    let masked = mask_phone(&cc, &national);
    budget.check(now).await?;

    let mut browser = Browser::connect(opts.cdp_base).await?;
    let ctx = browser.create_context().await?;
    let result = drive_send(browser, &ctx, opts, &cc, &national, &masked, budget).await;
    match result {
        Ok(p) => Ok(p),
        Err(e) => {
            dispose(opts.cdp_base, &ctx).await;
            Err(e)
        }
    }
}

async fn drive_send(
    mut browser: Browser,
    ctx: &str,
    opts: StartOpts<'_>,
    cc: &str,
    national: &str,
    masked: &str,
    budget: &dyn SendBudget,
) -> Result<PendingLogin> {
    let target = browser.open_tab(ctx, "about:blank").await?;
    let session = browser.attach(&target).await?;
    browser.navigate(&session, LOGIN_START_URL).await?;
    let mut tab = Tab { browser, session };

    let s = tab
        .wait_for(Duration::from_secs(45), |s| !matches!(s, Screen::Loading))
        .await?;
    if s != Screen::Phone {
        return Err(page_error(&s));
    }
    // Let reCAPTCHA / the fingerprint script settle like a person would.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let shown = tab.probe_text().await?;
    if !shown.contains(&format!("+{cc}")) {
        return Err(Error::invalid(format!(
            "the login page isn't set to +{cc}; only the page's default country is supported. Nothing was sent."
        )));
    }
    if tab
        .browser
        .eval(&tab.session, REMEMBER_STATE)
        .await?
        .as_str()
        == Some("unchecked")
    {
        tab.click(REMEMBER_LABEL, "remember-me checkbox").await?;
    }
    tab.click(PHONE_INPUT, "phone field").await?;
    tab.browser.type_digits(&tab.session, national).await?;
    let typed = tab
        .browser
        .eval(
            &tab.session,
            "(document.getElementById('Phone') || {}).value.replace(/\\D/g, '')",
        )
        .await?;
    if typed.as_str() != Some(national) {
        return Err(Error::new(
            ErrorKind::Api,
            "the phone field didn't take the number as typed; nothing was sent",
        ));
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    match tab.rect(CONTINUE_BTN).await? {
        Some((_, _, false)) => {}
        _ => {
            return Err(Error::new(
                ErrorKind::Api,
                "the Continue button is missing or disabled; nothing was sent",
            ));
        }
    }

    // ---- Point of no return: from here on a challenge may be sent. Record
    // the send first so a crash still counts against the limits.
    budget.record(jwt::now()).await?;
    tab.click(CONTINUE_BTN, "Continue button").await?;
    let mut s = tab
        .wait_for(Duration::from_secs(40), |s| {
            !matches!(s, Screen::Phone | Screen::Loading)
        })
        .await?;
    if let Screen::Options(offered) = &s {
        let Some(kind) = pick_option(opts.channel, offered) else {
            return Err(Error::invalid(format!(
                "Grab didn't offer {} for this account (offered: {}). Nothing was sent; the attempt still counts toward the limit.",
                opts.channel.as_str(),
                offered.join(", ")
            )));
        };
        let opt = format!(
            "(() => {{ const i = document.querySelector('[role=button] img[alt=\"Proceed to {kind}\"]'); return i ? i.closest('[role=button]') : null; }})()"
        );
        tab.click(&opt, "challenge option").await?;
        s = tab
            .wait_for(Duration::from_secs(40), |s| {
                !matches!(s, Screen::Options(_) | Screen::Loading)
            })
            .await?;
    }
    let (kind, channel_sent) = match &s {
        Screen::Otp { channel } => (PendingKind::OtpSent, channel.clone()),
        Screen::Approval => (PendingKind::ApprovalPending, Some("app".to_string())),
        Screen::Phone => {
            return Err(Error::new(
                ErrorKind::Api,
                "the login page stayed on the phone form after Continue (maybe a field error). Not retrying.",
            ));
        }
        other => return Err(page_error(other)),
    };
    let now = jwt::now();
    let p = PendingLogin {
        kind,
        cdp_base: opts.cdp_base.to_string(),
        browser_context_id: ctx.to_string(),
        target_id: target,
        created_at: now,
        expires_at: now + PENDING_TTL_SECS,
        phone_masked: masked.to_string(),
        channel_requested: opts.channel,
        channel_sent,
        copy_browser_address: opts.copy_browser_address,
    };
    Ok(p)
}

impl Tab {
    async fn probe_text(&mut self) -> Result<String> {
        let v = self.browser.eval(&self.session, PROBE_JS).await?;
        Ok(v.get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }
}

/// Before [`start`]: refuse while an unexpired login is pending; dispose an
/// expired one (and clear `slot`).
pub async fn check_no_pending(slot: &mut Option<PendingLogin>, now: i64) -> Result<()> {
    if let Some(p) = slot.as_ref() {
        if !p.is_expired(now) {
            return Err(Error::invalid(format!(
                "a login for {} is already pending ({}, {}s left): finish it with `grab auth login --otp <code>` / `--poll`, or run `grab auth login --cancel`",
                p.phone_masked,
                p.kind.as_str(),
                p.expires_at - now
            )));
        }
        dispose(&p.cdp_base, &p.browser_context_id).await;
        *slot = None;
    }
    Ok(())
}

/// Reattach to the pending login's tab, or clean up (clearing `slot`) and
/// explain.
async fn reattach(slot: &mut Option<PendingLogin>) -> Result<(Tab, PendingLogin)> {
    let p = slot.clone().ok_or_else(no_pending)?;
    if p.is_expired(jwt::now()) {
        dispose(&p.cdp_base, &p.browser_context_id).await;
        *slot = None;
        return Err(Error::new(
            ErrorKind::LoginExpired,
            "the pending login expired; start again with `grab auth login --phone <number>` (mind the send limits)",
        ));
    }
    let mut browser = Browser::connect(&p.cdp_base).await?;
    if !browser.context_exists(&p.browser_context_id).await?
        || !browser
            .tab_in_context(&p.browser_context_id, &p.target_id)
            .await?
    {
        *slot = None;
        return Err(Error::new(
            ErrorKind::LoginExpired,
            "the login tab is gone (browser restarted or tab closed); start again with `grab auth login --phone <number>`",
        ));
    }
    let session = browser.attach(&p.target_id).await?;
    Ok((Tab { browser, session }, p))
}

/// Poll the context's cookies for a logged-in `gfc_session`; return them as
/// the session (on top of `prev`), dispose the context and clear `slot`.
async fn collect_session(
    tab: &mut Tab,
    p: &PendingLogin,
    slot: &mut Option<PendingLogin>,
    prev: &Session,
) -> Result<Session> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let cookies = tab.browser.context_cookies(&p.browser_context_id).await?;
        let mut s = prev.clone();
        s.cookies = cookies;
        if s.gfc_logged_in() {
            // The isolated context has no saved address/country/language:
            // keep the previous session's, else (if allowed) take the address
            // from the browser's normal (default) context. Values stay in
            // memory.
            s.carry_over_preferences(prev);
            if p.copy_browser_address
                && s.cookie(LOCATION_COOKIE).is_none()
                && let Ok(def) = tab.browser.default_cookies().await
            {
                s.replace_named_cookies(LOCATION_COOKIE, &def);
            }
            if !tab.browser.user_agent.is_empty() {
                s.user_agent = Some(tab.browser.user_agent.clone());
            }
            s.exported_at = Some(jwt::now() as f64);
            tab.browser
                .dispose_context(&p.browser_context_id)
                .await
                .ok();
            *slot = None;
            return Ok(s);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::Api,
                "the login page finished but food.grab.com set no logged-in gfc_session; the tab is kept until the pending login expires",
            ));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// `grab auth login --otp <code>`: type the code into the waiting page.
/// `prev` is the session to build on (its preference cookies are kept);
/// pass `&Session::default()` for none. On success (and on a terminal
/// failure) `slot` is cleared; a rejected code leaves it for another try.
pub async fn finish_with_otp(
    slot: &mut Option<PendingLogin>,
    otp: &str,
    prev: &Session,
) -> Result<Session> {
    let otp = otp.trim();
    if !(4..=8).contains(&otp.len()) || !otp.chars().all(|c| c.is_ascii_digit()) {
        return Err(Error::invalid("the code should be 4-8 digits"));
    }
    let kind = slot.as_ref().ok_or_else(no_pending)?.kind;
    if kind != PendingKind::OtpSent {
        return Err(Error::invalid(
            "this login is waiting for an in-app approval: run `grab auth login --poll`",
        ));
    }
    let (mut tab, p) = reattach(slot).await?;
    match tab.probe().await? {
        Screen::Otp { .. } | Screen::OtpRejected => {}
        Screen::Done => return collect_session(&mut tab, &p, slot, prev).await,
        other => return Err(finish_error(&p, slot, &other).await),
    }
    tab.click(OTP_INPUT, "code field").await?;
    tab.browser.type_digits(&tab.session, otp).await?;
    // The page submits by itself once the code is complete.
    let s = tab
        .wait_for(Duration::from_secs(45), |s| {
            !matches!(s, Screen::Otp { .. } | Screen::Loading)
        })
        .await?;
    match s {
        Screen::Done => collect_session(&mut tab, &p, slot, prev).await,
        Screen::OtpRejected => Err(Error::new(
            ErrorKind::OtpInvalid,
            "Grab didn't accept the code; check it and run `grab auth login --otp <code>` again (the page allows only a few tries)",
        )),
        Screen::Otp { .. } | Screen::Loading => {
            // Maybe the redirect is just slow: check the cookies anyway.
            collect_session(&mut tab, &p, slot, prev).await
        }
        other => Err(finish_error(&p, slot, &other).await),
    }
}

/// Outcome of [`wait_for_approval`].
#[derive(Debug)]
pub enum WaitOutcome {
    LoggedIn(Session),
    /// Still waiting; the pending login is unchanged in the slot.
    StillWaiting,
}

/// `grab auth login --poll`: wait up to `timeout` for the in-app approval.
/// Same `slot` / `prev` rules as [`finish_with_otp`].
pub async fn wait_for_approval(
    slot: &mut Option<PendingLogin>,
    timeout: Duration,
    prev: &Session,
) -> Result<WaitOutcome> {
    let kind = slot.as_ref().ok_or_else(no_pending)?.kind;
    if kind != PendingKind::ApprovalPending {
        return Err(Error::invalid(
            "this login is waiting for a code: run `grab auth login --otp <code>`",
        ));
    }
    let (mut tab, p) = reattach(slot).await?;
    let left = u64::try_from(p.expires_at - jwt::now()).unwrap_or(0);
    let timeout = timeout.min(Duration::from_secs(left));
    let s = tab
        .wait_for(timeout, |s| {
            !matches!(s, Screen::Approval | Screen::Loading)
        })
        .await?;
    match s {
        Screen::Done => Ok(WaitOutcome::LoggedIn(
            collect_session(&mut tab, &p, slot, prev).await?,
        )),
        Screen::Approval | Screen::Loading => Ok(WaitOutcome::StillWaiting),
        other => Err(finish_error(&p, slot, &other).await),
    }
}

/// A terminal page state while finishing: clean up and map the error.
async fn finish_error(p: &PendingLogin, slot: &mut Option<PendingLogin>, s: &Screen) -> Error {
    dispose(&p.cdp_base, &p.browser_context_id).await;
    *slot = None;
    page_error(s)
}

fn no_pending() -> Error {
    Error::new(
        ErrorKind::LoginExpired,
        "no login in progress: start one with `grab auth login --phone <number>`",
    )
}

/// `grab auth login --cancel`: dispose the isolated context, forget the
/// pending login (clearing `slot`). Send limits are kept. Returns whether
/// there was one.
pub async fn cancel(slot: &mut Option<PendingLogin>) -> bool {
    let Some(p) = slot.take() else {
        return false;
    };
    dispose(&p.cdp_base, &p.browser_context_id).await;
    true
}

/// Order-tracking tabs in the shared browser (best-effort; empty if the
/// browser is unreachable). `/json` can still list a tab for a moment after
/// it was closed, so a non-empty answer is re-checked once after a short
/// delay and only tabs seen both times are reported.
pub async fn shared_browser_tabs(cdp_base: &str) -> Vec<String> {
    let first = cdp::pages(cdp_base)
        .await
        .map(|p| order_tracking_tabs(&p))
        .unwrap_or_default();
    if first.is_empty() {
        return first;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let second = cdp::pages(cdp_base)
        .await
        .map(|p| order_tracking_tabs(&p))
        .unwrap_or_default();
    still_open(&first, &second)
}

/// Tabs present in both listings.
pub fn still_open(first: &[String], second: &[String]) -> Vec<String> {
    first
        .iter()
        .filter(|t| second.contains(t))
        .cloned()
        .collect()
}

// ------------------------------------------------------------ step driver

/// What a [`LoginDriver`] is waiting for, or how it ended. Carries no phone
/// number or code (only the masked phone) and redacts the session in `Debug`.
#[derive(Debug, Clone)]
pub enum LoginState {
    /// A code was sent by SMS/WhatsApp: call [`LoginDriver::submit_code`].
    /// `error` is set after a rejected code (another try is allowed).
    AwaitingCode {
        phone_masked: String,
        /// Delivery Grab reported (`SMS`, `WhatsApp`), if known.
        channel: Option<String>,
        expires_at: i64,
        error: Option<String>,
    },
    /// An approval request went to the Grab app: call [`LoginDriver::poll`].
    AwaitingApproval {
        phone_masked: String,
        expires_at: i64,
    },
    /// Logged in. The session holds the captured cookies; nothing was saved
    /// anywhere: persist it through your own store.
    Done(Session),
    /// Ended for good (step-up, expiry, rate limit, page error, cancel).
    Failed(String),
}

impl LoginState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Failed(_))
    }
}

/// What to start a [`LoginDriver`] with.
#[derive(Clone)]
pub struct LoginRequest<'a> {
    /// CDP endpoint of a headed browser (e.g. `http://127.0.0.1:9222`).
    pub cdp_url: &'a str,
    /// International form (`+63…`). Used only to type into the page; never
    /// stored, logged or returned (only masked).
    pub phone: &'a str,
    pub channel: Channel,
    /// The previous session for this account, if any: its address/country/
    /// language cookies are carried over.
    pub previous: Option<Session>,
    /// Copy the address from the browser's default context when there is no
    /// previous one. Keep `false` for a browser shared between accounts.
    pub copy_browser_address: bool,
}

impl std::fmt::Debug for LoginRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRequest")
            .field("cdp_url", &self.cdp_url)
            .field("phone", &"<redacted>")
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

/// A phone login as a state machine, for a daemon: [`start`](Self::start)
/// sends ONE challenge (after the injected [`SendBudget`] allows it), then
/// [`submit_code`](Self::submit_code) or [`poll`](Self::poll) until the
/// state is terminal, or [`cancel`](Self::cancel). Writes no files and logs
/// nothing. The isolated browser context is disposed when the login ends;
/// call `cancel` when abandoning a driver so it is disposed promptly (an
/// abandoned one is otherwise only cleaned up by the browser).
#[derive(Debug)]
pub struct LoginDriver {
    pending: Option<PendingLogin>,
    previous: Session,
    state: LoginState,
}

fn failed(e: &Error) -> LoginState {
    LoginState::Failed(format!("{}: {}", e.kind.as_str(), e.message))
}

impl LoginDriver {
    /// Open the login page in a fresh isolated context and send one
    /// challenge. Never fails: errors become [`LoginState::Failed`].
    pub async fn start(req: LoginRequest<'_>, budget: &dyn SendBudget) -> Self {
        let previous = req.previous.clone().unwrap_or_default();
        let opts = StartOpts {
            cdp_base: req.cdp_url,
            phone: req.phone,
            channel: req.channel,
            copy_browser_address: req.copy_browser_address,
        };
        match start(opts, budget).await {
            Ok(p) => {
                let state = Self::waiting(&p, None);
                Self {
                    pending: Some(p),
                    previous,
                    state,
                }
            }
            Err(e) => Self {
                pending: None,
                previous,
                state: failed(&e),
            },
        }
    }

    fn waiting(p: &PendingLogin, error: Option<String>) -> LoginState {
        match p.kind {
            PendingKind::OtpSent => LoginState::AwaitingCode {
                phone_masked: p.phone_masked.clone(),
                channel: p.channel_sent.clone(),
                expires_at: p.expires_at,
                error,
            },
            PendingKind::ApprovalPending => LoginState::AwaitingApproval {
                phone_masked: p.phone_masked.clone(),
                expires_at: p.expires_at,
            },
        }
    }

    pub fn state(&self) -> &LoginState {
        &self.state
    }

    /// The pending login (no secrets), e.g. to show its expiry.
    pub fn pending(&self) -> Option<&PendingLogin> {
        self.pending.as_ref()
    }

    /// After an error: still waiting if the login is still pending (e.g. a
    /// rejected code), else failed.
    fn after_error(&mut self, e: &Error) {
        self.state = match &self.pending {
            Some(p) => Self::waiting(p, Some(e.message.clone())),
            None => failed(e),
        };
    }

    /// Type `code` into the page. Only valid while awaiting a code.
    pub async fn submit_code(&mut self, code: &str) -> &LoginState {
        if !matches!(self.state, LoginState::AwaitingCode { .. }) {
            return &self.state;
        }
        match finish_with_otp(&mut self.pending, code, &self.previous).await {
            Ok(s) => self.state = LoginState::Done(s),
            Err(e) => self.after_error(&e),
        }
        &self.state
    }

    /// Wait up to `wait` for the in-app approval (keep it short and call
    /// again; e.g. 5 s). Only valid while awaiting approval.
    pub async fn poll(&mut self, wait: Duration) -> &LoginState {
        if !matches!(self.state, LoginState::AwaitingApproval { .. }) {
            return &self.state;
        }
        match wait_for_approval(&mut self.pending, wait, &self.previous).await {
            Ok(WaitOutcome::LoggedIn(s)) => self.state = LoginState::Done(s),
            Ok(WaitOutcome::StillWaiting) => {}
            Err(e) => self.after_error(&e),
        }
        &self.state
    }

    /// Dispose the isolated context and end the login (the send still
    /// counts against the budget).
    pub async fn cancel(&mut self) -> &LoginState {
        if !self.state.is_terminal() {
            cancel(&mut self.pending).await;
            self.state = LoginState::Failed("cancelled".into());
        }
        &self.state
    }

    /// The session once [`LoginState::Done`].
    pub fn into_session(self) -> Option<Session> {
        match self.state {
            LoginState::Done(s) => Some(s),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_and_masks_phone() -> Result<()> {
        let (cc, n) = split_phone("+63 900 000 0012")?;
        assert_eq!((cc.as_str(), n.as_str()), ("63", "9000000012"));
        assert_eq!(mask_phone(&cc, &n), "+63••••••••12");
        assert!(!mask_phone(&cc, &n).contains("900"));
        let (cc, n) = split_phone("+65-8000-0000")?;
        assert_eq!((cc.as_str(), n.as_str()), ("65", "80000000"));
        assert!(split_phone("09000000000").is_err());
        assert!(split_phone("+1 555 000 0000").is_err());
        assert!(split_phone("+63 12").is_err());
        assert_eq!(mask_phone("63", "1"), "+631");
        Ok(())
    }

    #[test]
    fn channels() -> Result<()> {
        assert_eq!(Channel::parse("APP")?, Channel::App);
        assert_eq!(Channel::parse("whatsapp")?, Channel::Whatsapp);
        assert!(Channel::parse("email").is_err());
        let offered = vec![
            "PUSH_V2".to_string(),
            "CHANNEL_OPTIMIZED_PHONE_OTP".to_string(),
        ];
        assert_eq!(pick_option(Channel::App, &offered), Some("PUSH_V2"));
        assert_eq!(
            pick_option(Channel::Sms, &offered),
            Some("CHANNEL_OPTIMIZED_PHONE_OTP")
        );
        assert_eq!(pick_option(Channel::Sms, &["PUSH".to_string()]), None);
        Ok(())
    }

    #[test]
    fn cooldown_and_hourly_cap() {
        let mut log = SendLog::default();
        let t0 = 1_000_000;
        assert!(log.check(t0).is_ok());
        log.record(t0);
        // Cooldown.
        assert_eq!(log.wait_secs(t0 + 10), 50);
        let e = log.check(t0 + 10).err();
        assert_eq!(e.as_ref().map(|e| e.kind), Some(ErrorKind::RateLimited));
        assert!(log.check(t0 + 60).is_ok());
        log.record(t0 + 60);
        log.record(t0 + 200);
        // Three in the hour: blocked until the first one ages out.
        assert_eq!(log.sends_last_hour(t0 + 300), 3);
        assert_eq!(log.wait_secs(t0 + 300), 3600 - 300);
        assert!(log.check(t0 + 3599).is_err());
        assert!(log.check(t0 + 3600).is_ok());
        // Recording prunes entries older than an hour.
        log.record(t0 + 3700);
        assert_eq!(log.sends.len(), 2);
    }

    fn pending() -> PendingLogin {
        PendingLogin {
            kind: PendingKind::OtpSent,
            cdp_base: "http://127.0.0.1:9222".into(),
            browser_context_id: "CTX".into(),
            target_id: "TAB".into(),
            created_at: 100,
            expires_at: 100 + PENDING_TTL_SECS,
            phone_masked: mask_phone("63", "9000000012"),
            channel_requested: Channel::Sms,
            channel_sent: Some("SMS".into()),
            copy_browser_address: false,
        }
    }

    #[test]
    fn pending_serializes_without_phone() -> Result<()> {
        let p = pending();
        let raw = serde_json::to_string(&p)?;
        assert!(!raw.contains("9000000012"));
        let back: PendingLogin = serde_json::from_str(&raw)?;
        assert_eq!(back.kind, PendingKind::OtpSent);
        assert!(!back.copy_browser_address);
        assert!(!back.is_expired(200));
        assert!(back.is_expired(100 + PENDING_TTL_SECS));
        assert_eq!(back.summary(160)["expires_in_secs"], json!(540));
        let j = pending_json(&back, 160);
        assert_eq!(j["kind"], json!("otp_sent"));
        assert_eq!(j["expires_at"], json!(100 + PENDING_TTL_SECS));
        assert_eq!(j["next"], json!("grab auth login --otp <code>"));
        let approval = PendingLogin {
            kind: PendingKind::ApprovalPending,
            ..back
        };
        assert_eq!(
            pending_json(&approval, 160)["next"],
            json!("grab auth login --poll")
        );
        // Older pending files (before the flag) keep copying the address.
        let mut v = serde_json::to_value(&p)?;
        v.as_object_mut().map(|o| o.remove("copy_browser_address"));
        assert!(serde_json::from_value::<PendingLogin>(v)?.copy_browser_address);
        Ok(())
    }

    #[tokio::test]
    async fn memory_budget_enforces_limits() -> Result<()> {
        let b = MemorySendBudget::default();
        b.check(1000).await?;
        b.record(1000).await?;
        let e = b.check(1010).await.err();
        assert_eq!(e.map(|e| e.kind), Some(ErrorKind::RateLimited));
        assert_eq!(b.snapshot().sends, vec![1000]);
        Ok(())
    }

    #[tokio::test]
    async fn driver_never_sends_when_budget_is_spent() -> Result<()> {
        let b = MemorySendBudget::default();
        b.record(jwt::now()).await?;
        // The budget refuses before any browser is contacted (the CDP URL
        // is unroutable, so reaching it would fail differently).
        let d = LoginDriver::start(
            LoginRequest {
                cdp_url: "http://127.0.0.1:1",
                phone: "+63 900 000 0012",
                channel: Channel::App,
                previous: None,
                copy_browser_address: false,
            },
            &b,
        )
        .await;
        match d.state() {
            LoginState::Failed(r) => {
                assert!(r.starts_with("rate_limited"), "{r}");
                assert!(!r.contains("0012"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(b.snapshot().sends.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn driver_rejects_bad_phone_without_sending() {
        let b = MemorySendBudget::default();
        let mut d = LoginDriver::start(
            LoginRequest {
                cdp_url: "http://127.0.0.1:1",
                phone: "0900",
                channel: Channel::Sms,
                previous: None,
                copy_browser_address: false,
            },
            &b,
        )
        .await;
        assert!(matches!(d.state(), LoginState::Failed(_)));
        assert!(b.snapshot().sends.is_empty());
        // Steps on a terminal driver are no-ops.
        assert!(matches!(
            d.submit_code("123456").await,
            LoginState::Failed(_)
        ));
        assert!(matches!(d.cancel().await, LoginState::Failed(_)));
        assert!(d.into_session().is_none());
    }

    #[test]
    fn dry_run_sends_nothing_and_reports_limits() {
        let mut log = SendLog::default();
        log.record(1000);
        let v = dry_run_plan("+63••••••••12", Channel::App, "http://x:9222", &log, 1030);
        assert_eq!(v["kind"], json!("dry_run"));
        assert_eq!(v["dry_run"], json!(true));
        assert_eq!(v["sent"], json!(false));
        assert_eq!(v["phone"], json!("+63••••••••12"));
        assert_eq!(v["sends_last_hour"], json!(1));
        assert_eq!(v["next_send_allowed_in_secs"], json!(30));
        assert!(
            v["would_send"]
                .as_str()
                .unwrap_or_default()
                .contains("approval")
        );
    }

    fn probe(host: &str, path: &str, text: &str) -> Value {
        json!({"host": host, "path": path, "text": text, "has_phone": false, "options": [], "captcha": false})
    }

    #[test]
    fn classifies_screens() {
        let mut phone = probe(
            "weblogin.grab.com",
            "/passenger/login",
            "Sign in to Grab\n+63",
        );
        phone["has_phone"] = json!(true);
        assert_eq!(classify_screen(&phone), Screen::Phone);
        assert_eq!(
            classify_screen(&probe("weblogin.grab.com", "/passenger/login", "")),
            Screen::Loading
        );
        let mut opts = probe(
            "weblogin.grab.com",
            "/passenger/challenge-options",
            "Choose a preferred way to sign in",
        );
        opts["options"] = json!(["PUSH_V2", "CHANNEL_OPTIMIZED_PHONE_OTP"]);
        assert_eq!(
            classify_screen(&opts),
            Screen::Options(vec!["PUSH_V2".into(), "CHANNEL_OPTIMIZED_PHONE_OTP".into()])
        );
        assert_eq!(
            classify_screen(&probe(
                "weblogin.grab.com",
                "/passenger/optimised-sms-otp",
                "Check your WhatsApp\nWe've sent a verification code"
            )),
            Screen::Otp {
                channel: Some("WhatsApp".into())
            }
        );
        assert_eq!(
            classify_screen(&probe(
                "weblogin.grab.com",
                "/passenger/push-v2",
                "Approve in mobile app"
            )),
            Screen::Approval
        );
        assert_eq!(
            classify_screen(&probe(
                "weblogin.grab.com",
                "/passenger/optimised-sms-otp",
                "Incorrect code. You can try 2 more times."
            )),
            Screen::OtpRejected
        );
        assert!(matches!(
            classify_screen(&probe(
                "weblogin.grab.com",
                "/passenger/x",
                "Account temporarily blocked"
            )),
            Screen::Problem(_)
        ));
        assert_eq!(
            classify_screen(&probe("weblogin.grab.com", "/passenger/web-selfie", "")),
            Screen::StepUp("web-selfie".into())
        );
        let mut cap = probe("weblogin.grab.com", "/passenger/login", "");
        cap["captcha"] = json!(true);
        assert_eq!(classify_screen(&cap), Screen::StepUp("recaptcha".into()));
        assert_eq!(
            classify_screen(&probe("food.grab.com", "/ph/en/", "")),
            Screen::Done
        );
        assert_eq!(
            classify_screen(&probe("food.grab.com", "/auth/callback", "")),
            Screen::Loading
        );
        // Text fallback if routes are renamed.
        assert_eq!(
            classify_screen(&probe(
                "weblogin.grab.com",
                "/passenger/new",
                "Check your SMS"
            )),
            Screen::Otp {
                channel: Some("SMS".into())
            }
        );
    }

    #[test]
    fn finds_tracking_tabs() {
        let pages = vec![
            PageInfo {
                id: "1".into(),
                url: "https://food.grab.com/ph/en/order/001-ABC".into(),
            },
            PageInfo {
                id: "2".into(),
                url: "https://food.grab.com/ph/en/restaurants".into(),
            },
            PageInfo {
                id: "3".into(),
                url: "https://example.com/order/1".into(),
            },
        ];
        let tabs = order_tracking_tabs(&pages);
        assert_eq!(tabs, vec!["food.grab.com/ph/en/order/001-ABC".to_string()]);
        assert!(tracking_tab_warning(&tabs).is_some_and(|w| w.contains("logout")));
        assert!(tracking_tab_warning(&[]).is_none());
    }

    #[test]
    fn stale_tabs_are_dropped() {
        let a = "food.grab.com/ph/en/order/A".to_string();
        let b = "food.grab.com/ph/en/order/B".to_string();
        assert_eq!(
            still_open(&[a.clone(), b.clone()], std::slice::from_ref(&b)),
            vec![b.clone()]
        );
        assert!(still_open(&[a], &[]).is_empty());
    }
}
