//! Chrome DevTools Protocol client.
//!
//! Talks to a logged-in Chrome/Chromium exposing CDP (default
//! `http://127.0.0.1:9222`) to do two things grabcli can't do with HTTP alone:
//!
//! - [`export_cookies`]: read the browser's Grab cookies (`Storage.getCookies`)
//!   so the user never pastes credentials.
//! - [`mint_hydra`]: open a `food.grab.com` page in the browser, let it mint a
//!   real `x-hydra-jwt`, and sniff the header off the network events.
//! - [`evaluate_in_origin`]: evaluate JS in a tab on a given origin (used to
//!   mint GrabMart reCAPTCHA v3 tokens on `mart.grab.com`).
//! - [`Browser`]: drive one tab inside an isolated browser context
//!   (`Target.createBrowserContext`) for `grab auth login`; the context has
//!   its own cookies and outlives the process until disposed.
//!
//! The endpoint is any CDP-speaking browser the user is logged into; grabcli
//! doesn't assume a particular one. It only drives a tab it opens or a tab
//! already on `food.grab.com`, and never touches unrelated tabs.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::error::{Error, ErrorKind, Result};
use crate::jwt;
use crate::session::{Cookie, Session};

fn cdp_err(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::CdpUnavailable, msg.into())
}

/// `http://host:port` -> the browser-level websocket debugger URL + User-Agent.
async fn version(base: &str) -> Result<(String, String)> {
    let url = format!("{}/json/version", base.trim_end_matches('/'));
    let v: Value = reqwest::get(&url)
        .await
        .map_err(|e| cdp_err(format!("CDP {url}: {e}")))?
        .json()
        .await
        .map_err(|e| cdp_err(format!("CDP {url}: {e}")))?;
    let ws = v
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| cdp_err("CDP /json/version has no webSocketDebuggerUrl"))?
        .to_string();
    let ua = v
        .get("User-Agent")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((ws, ua))
}

async fn list_pages(base: &str) -> Result<Vec<Value>> {
    let url = format!("{}/json", base.trim_end_matches('/'));
    let v: Value = reqwest::get(&url)
        .await
        .map_err(|e| cdp_err(format!("CDP {url}: {e}")))?
        .json()
        .await
        .map_err(|e| cdp_err(format!("CDP {url}: {e}")))?;
    Ok(v.as_array().cloned().unwrap_or_default())
}

/// A thin CDP websocket connection with monotonically increasing ids.
struct Conn {
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
    /// Events read while waiting for a reply, for [`Conn::next_event`].
    events: std::collections::VecDeque<Value>,
}

impl Conn {
    async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .map_err(|e| cdp_err(format!("CDP websocket {ws_url}: {e}")))?;
        Ok(Self {
            ws,
            next_id: 0,
            events: std::collections::VecDeque::new(),
        })
    }

    async fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        self.send_to(None, method, params).await
    }

    /// Send to a flattened target session (`Target.attachToTarget`), or to
    /// the connection itself when `session` is `None`.
    async fn send_to(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.ws
            .send(Message::Text(msg.to_string().into()))
            .await
            .map_err(|e| cdp_err(format!("CDP send {method}: {e}")))?;
        Ok(id)
    }

    /// Read the reply to request `id`, ignoring events and other replies.
    async fn result_for(&mut self, id: u64, timeout: Duration) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(cdp_err("timed out waiting for CDP reply"));
            }
            let msg = tokio::time::timeout(remaining, self.ws.next())
                .await
                .map_err(|_| cdp_err("timed out waiting for CDP reply"))?
                .ok_or_else(|| cdp_err("CDP connection closed"))?
                .map_err(|e| cdp_err(format!("CDP recv: {e}")))?;
            if let Message::Text(txt) = msg {
                let v: Value = serde_json::from_str(&txt)?;
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(err) = v.get("error") {
                        return Err(cdp_err(format!("CDP error: {err}")));
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                if v.get("method").is_some() && self.events.len() < 10_000 {
                    self.events.push_back(v);
                }
            }
        }
    }

    /// Send and wait for the reply.
    async fn call(&mut self, session: Option<&str>, method: &str, params: Value) -> Result<Value> {
        let id = self.send_to(session, method, params).await?;
        self.result_for(id, Duration::from_secs(20)).await
    }

    /// Next event (buffered first), or `None` after `timeout`.
    async fn next_event(&mut self, timeout: Duration) -> Result<Option<Value>> {
        if let Some(v) = self.events.pop_front() {
            return Ok(Some(v));
        }
        loop {
            match tokio::time::timeout(timeout, self.ws.next()).await {
                Err(_) => return Ok(None),
                Ok(None) => return Err(cdp_err("CDP connection closed")),
                Ok(Some(Err(e))) => return Err(cdp_err(format!("CDP recv: {e}"))),
                Ok(Some(Ok(Message::Text(t)))) => {
                    let v: Value = serde_json::from_str(&t)?;
                    if v.get("method").is_some() {
                        return Ok(Some(v));
                    }
                }
                Ok(Some(Ok(_))) => {}
            }
        }
    }
}

/// Convert CDP `Network.Cookie`s to session cookies, keeping only grab.com.
fn grab_cookies(raw: &[Value]) -> Vec<Cookie> {
    raw.iter()
        .filter_map(|c| {
            let domain = c.get("domain").and_then(Value::as_str)?;
            let bare = domain.trim_start_matches('.');
            if !(bare == "grab.com" || bare.ends_with(".grab.com")) {
                return None;
            }
            Some(Cookie {
                name: c.get("name").and_then(Value::as_str)?.to_string(),
                value: c.get("value").and_then(Value::as_str)?.to_string(),
                domain: domain.to_string(),
                path: c
                    .get("path")
                    .and_then(Value::as_str)
                    .unwrap_or("/")
                    .to_string(),
                expires: c.get("expires").and_then(Value::as_f64).unwrap_or(-1.0),
            })
        })
        .collect()
}

/// A page target as listed by `/json` (no websocket needed).
#[derive(Debug, Clone)]
pub struct PageInfo {
    pub id: String,
    pub url: String,
}

/// All `page` targets of the browser, from `GET /json`. Read-only: nothing
/// is attached to or changed.
pub async fn pages(base: &str) -> Result<Vec<PageInfo>> {
    Ok(list_pages(base)
        .await?
        .iter()
        .filter(|p| p.get("type").and_then(Value::as_str) == Some("page"))
        .filter_map(|p| {
            Some(PageInfo {
                id: p.get("id").and_then(Value::as_str)?.to_string(),
                url: p.get("url").and_then(Value::as_str)?.to_string(),
            })
        })
        .collect())
}

/// A browser-level CDP connection used to drive one page inside an
/// **isolated browser context** (`Target.createBrowserContext`): its own
/// cookie jar and storage, shared with no other tab, so nothing else in the
/// browser can log it out and it can't disturb the user's tabs.
///
/// Contexts are created with `disposeOnDetach: false`, so the context and
/// its tab survive this process exiting; a later process reconnects by id.
pub struct Browser {
    conn: Conn,
    /// The browser's User-Agent.
    pub user_agent: String,
}

impl Browser {
    pub async fn connect(base: &str) -> Result<Self> {
        let (ws_url, ua) = version(base).await?;
        Ok(Self {
            conn: Conn::connect(&ws_url).await?,
            user_agent: ua,
        })
    }

    /// Raw CDP call, optionally on an attached page session.
    pub async fn call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.conn.call(session, method, params).await
    }

    /// Create an isolated context that outlives this connection.
    pub async fn create_context(&mut self) -> Result<String> {
        let r = self
            .call(
                None,
                "Target.createBrowserContext",
                json!({"disposeOnDetach": false}),
            )
            .await?;
        r.get("browserContextId")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| cdp_err("Target.createBrowserContext returned no id"))
    }

    pub async fn context_exists(&mut self, context: &str) -> Result<bool> {
        let r = self
            .call(None, "Target.getBrowserContexts", json!({}))
            .await?;
        Ok(r.get("browserContextIds")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().any(|i| i.as_str() == Some(context))))
    }

    /// Dispose a context we created (closes its tabs, drops its cookies).
    pub async fn dispose_context(&mut self, context: &str) -> Result<()> {
        self.call(
            None,
            "Target.disposeBrowserContext",
            json!({"browserContextId": context}),
        )
        .await
        .map(|_| ())
    }

    /// Open `url` in a new background tab of `context`.
    pub async fn open_tab(&mut self, context: &str, url: &str) -> Result<String> {
        let r = self
            .call(
                None,
                "Target.createTarget",
                json!({"url": url, "browserContextId": context, "background": true}),
            )
            .await?;
        r.get("targetId")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| cdp_err("Target.createTarget returned no id"))
    }

    /// Whether `target` is still a page in `context`.
    pub async fn tab_in_context(&mut self, context: &str, target: &str) -> Result<bool> {
        let r = self.call(None, "Target.getTargets", json!({})).await?;
        Ok(r.get("targetInfos")
            .and_then(Value::as_array)
            .is_some_and(|ts| {
                ts.iter().any(|t| {
                    t.get("targetId").and_then(Value::as_str) == Some(target)
                        && t.get("browserContextId").and_then(Value::as_str) == Some(context)
                })
            }))
    }

    /// Attach to a tab (flattened session) and make it behave like a focused,
    /// visible page: a background tab otherwise never runs
    /// `requestAnimationFrame`, and the login SPA stays on its spinner.
    pub async fn attach(&mut self, target: &str) -> Result<String> {
        let r = self
            .call(
                None,
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
            )
            .await?;
        let s = r
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| cdp_err("Target.attachToTarget returned no session"))?;
        self.call(Some(&s), "Page.enable", json!({})).await?;
        self.call(
            Some(&s),
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled": true}),
        )
        .await?;
        self.call(
            Some(&s),
            "Page.setWebLifecycleState",
            json!({"state": "active"}),
        )
        .await?;
        Ok(s)
    }

    /// Evaluate `expression` in the page (promises awaited) and return its
    /// JSON value. Callers must not log values that are credentials.
    pub async fn eval(&mut self, session: &str, expression: &str) -> Result<Value> {
        let r = self
            .call(
                Some(session),
                "Runtime.evaluate",
                json!({"expression": expression, "awaitPromise": true, "returnByValue": true}),
            )
            .await?;
        if let Some(ex) = r.get("exceptionDetails") {
            let text = ex
                .pointer("/exception/description")
                .or_else(|| ex.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("exception");
            return Err(cdp_err(format!("page evaluation failed: {text}")));
        }
        Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
    }

    /// Navigate the attached page.
    pub async fn navigate(&mut self, session: &str, url: &str) -> Result<()> {
        self.call(Some(session), "Page.navigate", json!({"url": url}))
            .await
            .map(|_| ())
    }

    /// A trusted left click at viewport coordinates (as a user's would be).
    pub async fn click_at(&mut self, session: &str, x: f64, y: f64) -> Result<()> {
        for (kind, buttons) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 0)] {
            self.call(
                Some(session),
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "buttons": buttons, "clickCount": 1}),
            )
            .await?;
        }
        Ok(())
    }

    /// Type ASCII digits into the focused element, one key at a time.
    pub async fn type_digits(&mut self, session: &str, digits: &str) -> Result<()> {
        for ch in digits.chars().filter(char::is_ascii_digit) {
            let key = ch.to_string();
            let vk = 48 + ch.to_digit(10).unwrap_or(0);
            for kind in ["keyDown", "keyUp"] {
                let mut p = json!({
                    "type": kind, "key": key, "code": format!("Digit{ch}"),
                    "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
                });
                if kind == "keyDown" {
                    p["text"] = json!(key);
                }
                self.call(Some(session), "Input.dispatchKeyEvent", p)
                    .await?;
            }
            tokio::time::sleep(Duration::from_millis(90)).await;
        }
        Ok(())
    }

    /// The grab.com cookies of one browser context.
    pub async fn context_cookies(&mut self, context: &str) -> Result<Vec<Cookie>> {
        let r = self
            .call(
                None,
                "Storage.getCookies",
                json!({"browserContextId": context}),
            )
            .await?;
        let raw = r
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(grab_cookies(&raw))
    }

    /// The grab.com cookies of the browser's default context (the user's
    /// normal tabs), as opposed to [`context_cookies`](Self::context_cookies).
    pub async fn default_cookies(&mut self) -> Result<Vec<Cookie>> {
        let r = self.call(None, "Storage.getCookies", json!({})).await?;
        let raw = r
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(grab_cookies(&raw))
    }

    /// Drain buffered CDP events (e.g. after a navigation), waiting up to
    /// `wait` for more. Returns how many were seen.
    pub async fn drain_events(&mut self, wait: Duration) -> Result<usize> {
        let mut n = 0;
        while self.conn.next_event(wait).await?.is_some() {
            n += 1;
        }
        Ok(n)
    }
}

/// The grab.com cookies of the browser's **default** context (the user's
/// normal tabs): `Storage.getCookies` on the browser connection without a
/// `browserContextId`. Values stay in memory; callers never print them.
pub async fn default_context_cookies(base: &str) -> Result<(Vec<Cookie>, String)> {
    let (ws_url, ua) = version(base).await?;
    let mut conn = Conn::connect(&ws_url).await?;
    let id = conn.send("Storage.getCookies", json!({})).await?;
    let result = conn.result_for(id, Duration::from_secs(10)).await?;
    let raw = result
        .get("cookies")
        .and_then(Value::as_array)
        .ok_or_else(|| cdp_err("Storage.getCookies returned no cookies"))?;
    Ok((grab_cookies(raw), ua))
}

/// Export Grab cookies from the browser into `session`, preserving any hydra
/// token already present. Returns the browser User-Agent.
pub async fn export_cookies(base: &str, session: &mut Session) -> Result<String> {
    let (cookies, ua) = default_context_cookies(base).await?;
    if cookies.is_empty() {
        return Err(cdp_err(
            "no grab.com cookies in the browser; is it logged in?",
        ));
    }
    session.cookies = cookies;
    if !ua.is_empty() {
        session.user_agent = Some(ua.clone());
    }
    session.exported_at = Some(jwt::now() as f64);
    Ok(ua)
}

/// Evaluate a JavaScript `expression` in a browser tab on `origin` (e.g.
/// `https://mart.grab.com`) and return its JSON value. Promises are awaited.
///
/// Reuses an existing tab whose URL starts with `origin`; otherwise opens
/// `open_url` in a new background tab (left open so later calls reuse it). It
/// never navigates or evaluates in any other tab. Used to mint page-bound
/// tokens such as GrabMart's reCAPTCHA v3 token; callers must not log the
/// returned value when it is a credential.
pub async fn evaluate_in_origin(
    base: &str,
    origin: &str,
    open_url: &str,
    expression: &str,
    timeout: Duration,
) -> Result<Value> {
    let on_origin = |p: &Value| {
        p.get("type").and_then(Value::as_str) == Some("page")
            && p.get("webSocketDebuggerUrl").is_some()
            && p.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with(origin))
    };
    let pages = list_pages(base).await?;
    let target = match pages.iter().find(|p| on_origin(p)) {
        Some(p) => p.clone(),
        None => {
            // Chrome wants PUT for /json/new.
            let url = format!(
                "{}/json/new?{}",
                base.trim_end_matches('/'),
                urlencoding::encode(open_url)
            );
            reqwest::Client::new()
                .put(&url)
                .send()
                .await
                .map_err(|e| cdp_err(format!("CDP /json/new: {e}")))?
                .json::<Value>()
                .await
                .map_err(|e| cdp_err(format!("CDP /json/new: {e}")))?
        }
    };
    let ws_url = target
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| cdp_err("target tab has no websocket URL"))?;
    let mut conn = Conn::connect(ws_url).await?;

    let deadline = tokio::time::Instant::now() + timeout;
    let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
    // Wait until the tab is actually on `origin` (a fresh tab starts blank).
    loop {
        let id = conn
            .send(
                "Runtime.evaluate",
                json!({"expression": "location.origin + '|' + document.readyState", "returnByValue": true}),
            )
            .await?;
        let r = conn.result_for(id, left()).await?;
        let s = r
            .pointer("/result/value")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if s.starts_with(origin) && !s.ends_with("|loading") {
            break;
        }
        if left().is_zero() {
            return Err(cdp_err(format!("browser tab never reached {origin}")));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    let id = conn
        .send(
            "Runtime.evaluate",
            json!({"expression": expression, "awaitPromise": true, "returnByValue": true}),
        )
        .await?;
    let r = conn.result_for(id, left()).await?;
    if let Some(ex) = r.get("exceptionDetails") {
        let text = ex
            .pointer("/exception/description")
            .or_else(|| ex.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("exception");
        return Err(cdp_err(format!("browser evaluation failed: {text}")));
    }
    Ok(r.pointer("/result/value").cloned().unwrap_or(Value::Null))
}

/// Mint a hydra JWT by driving the browser: open/select a `food.grab.com` tab,
/// navigate it to `page`, and sniff the `x-hydra-jwt` request header. Updates
/// `session.hydra_jwt`, `hydra_exp` and `web_app_version`.
pub async fn mint_hydra(base: &str, page: &str, session: &mut Session) -> Result<()> {
    let pages = list_pages(base).await?;
    // Prefer an existing food.grab.com tab; else a tab we can navigate.
    let target = pages
        .iter()
        .find(|p| {
            p.get("type").and_then(Value::as_str) == Some("page")
                && p.get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|u| u.contains("food.grab.com"))
        })
        .or_else(|| {
            pages.iter().find(|p| {
                p.get("type").and_then(Value::as_str) == Some("page")
                    && p.get("webSocketDebuggerUrl").is_some()
            })
        })
        .ok_or_else(|| cdp_err("no usable browser tab found over CDP"))?;

    let ws_url = target
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| cdp_err("target tab has no websocket URL"))?;

    let mut conn = Conn::connect(ws_url).await?;
    conn.send("Network.enable", json!({})).await?;
    conn.send("Page.enable", json!({})).await?;
    conn.send("Page.navigate", json!({ "url": page })).await?;

    // Sniff request headers for up to 25s.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Error::new(
                ErrorKind::HydraExpired,
                "no x-hydra-jwt seen from the browser within 25s",
            ));
        }
        let msg = match tokio::time::timeout(remaining, conn.ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => t,
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => return Err(cdp_err(format!("CDP recv: {e}"))),
            Ok(None) => return Err(cdp_err("CDP connection closed")),
            Err(_) => continue,
        };
        let v: Value = serde_json::from_str(&msg)?;
        let method = v.get("method").and_then(Value::as_str).unwrap_or_default();
        if method != "Network.requestWillBeSent" && method != "Network.requestWillBeSentExtraInfo" {
            continue;
        }
        let headers = v
            .pointer("/params/request/headers")
            .or_else(|| v.pointer("/params/headers"));
        let Some(headers) = headers.and_then(Value::as_object) else {
            continue;
        };
        let find = |want: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(want))
                .and_then(|(_, val)| val.as_str())
                .map(ToString::to_string)
        };
        if let Some(jwt_tok) = find("x-hydra-jwt") {
            let exp = jwt::exp(&jwt_tok).unwrap_or_else(|| jwt::now() + 600);
            session.hydra_jwt = Some(jwt_tok);
            session.hydra_exp = Some(exp);
            if let Some(ver) = find("x-grab-web-app-version") {
                session.web_app_version = Some(ver);
            }
            return Ok(());
        }
    }
}
