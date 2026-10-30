//! `grab auth …`: import the browser session, mint hydra, show status.
//!
//! grabcli never creates Grab credentials. `import` copies them from a browser
//! the user is already logged into (over the Chrome DevTools Protocol), or from
//! an existing `session.json` the user points at.

use bpaf::Bpaf;
use grabcli_core::cache::CacheMode;
use grabcli_core::config::{Config, DEFAULT_CDP};
use grabcli_core::error::{Error, ErrorKind, Result};
use grabcli_core::session::Session;
use grabcli_core::{Allowlist, Context, cdp, login};
use serde_json::json;

use crate::local::{self, FileSendBudget, FileSessionStore};
use crate::output::Output;

/// A context over the saved session with no service allowlist.
async fn auth_context() -> Result<Context> {
    local::context(|_| Allowlist::new(), CacheMode::Off).await
}

/// The session the login builds on (its preference cookies are kept).
fn previous_session() -> Session {
    FileSessionStore::default_path()
        .and_then(|s| s.read())
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// `grab auth <cmd>`.
#[derive(Debug, Clone, Bpaf)]
pub enum AuthCmd {
    /// Log in. With no flags: silent SSO refresh, then (only if that fails and
    /// `phone` is set in config.toml) the same as `--phone <config phone>`.
    /// `--phone N`: drive the real weblogin page in an isolated browser
    /// context and send ONE challenge; finish with `--otp CODE` or `--poll`.
    /// `--phone N --dry-run` shows the plan and sends nothing.
    #[bpaf(command("login"))]
    Login {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Chrome DevTools Protocol endpoint of the browser to drive.
        #[bpaf(long("cdp"), argument("URL"))]
        cdp: Option<String>,
        /// Phone number in international form (+63…). Never stored in full.
        /// Default: `phone` from config.toml.
        #[bpaf(long("phone"), argument("NUMBER"))]
        phone: Option<String>,
        /// app (in-app approval), sms or whatsapp (an OTP code). Default: sms.
        #[bpaf(long("channel"), argument("CHANNEL"))]
        channel: Option<String>,
        /// With --phone: show what would happen; send nothing.
        #[bpaf(long("dry-run"), switch)]
        dry_run: bool,
        /// Finish with the code Grab sent ("-" reads it from stdin).
        #[bpaf(long("otp"), argument("CODE"))]
        otp: Option<String>,
        /// With --phone and an OTP channel, read the code from stdin in the
        /// same process.
        #[bpaf(long("otp-prompt"), switch)]
        otp_prompt: bool,
        /// Wait for the in-app approval (alone, or together with --phone).
        /// `--wait` is accepted as an alias.
        #[bpaf(long("poll"), long("wait"), switch)]
        poll: bool,
        /// Seconds to wait with --poll (default 300, capped by the login's
        /// remaining lifetime).
        #[bpaf(long("timeout"), argument("SECS"))]
        timeout: Option<u64>,
        /// Abandon the pending login: close its isolated browser context.
        #[bpaf(long("cancel"), switch)]
        cancel: bool,
    },

    /// Import a session: from a browser over CDP, or from an existing file.
    #[bpaf(command("import"))]
    Import {
        #[bpaf(long("json"), switch)]
        json: bool,
        /// Chrome DevTools Protocol endpoint of a logged-in browser.
        #[bpaf(long("cdp"), argument("URL"))]
        cdp: Option<String>,
        /// Import an existing session.json instead of using a browser.
        #[bpaf(long("from"), argument("PATH"))]
        from: Option<std::path::PathBuf>,
        /// Copy only the saved delivery address (the `location` cookie) from
        /// the browser; keep every other cookie, including the login.
        #[bpaf(long("address-only"), switch)]
        address_only: bool,
    },

    /// Refresh gfc_session silently through the SSO cookie (no browser, never
    /// sends a challenge).
    #[bpaf(command("refresh"))]
    Refresh {
        #[bpaf(long("json"), switch)]
        json: bool,
    },

    /// Mint (or refresh) the hydra device JWT.
    #[bpaf(command("hydra"))]
    Hydra {
        #[bpaf(long("json"), switch)]
        json: bool,
    },

    /// Show session status: cookie expiries, hydra, saved address — no values.
    #[bpaf(command("status"))]
    Status {
        #[bpaf(long("json"), switch)]
        json: bool,
    },
}

pub fn wants_json(cmd: &AuthCmd) -> bool {
    match cmd {
        AuthCmd::Login { json, .. }
        | AuthCmd::Import { json, .. }
        | AuthCmd::Refresh { json, .. }
        | AuthCmd::Hydra { json, .. }
        | AuthCmd::Status { json, .. } => *json,
    }
}

fn cdp_base(cli: Option<&str>, config: &Config) -> String {
    cli.map(ToString::to_string)
        .or_else(|| config.cdp.clone())
        .unwrap_or_else(|| DEFAULT_CDP.to_string())
}

pub async fn run(out: Output, cmd: AuthCmd) -> Result<()> {
    let config = local::load_config()?;
    match cmd {
        AuthCmd::Login {
            cdp,
            phone,
            channel,
            dry_run,
            otp,
            otp_prompt,
            poll,
            timeout,
            cancel,
            ..
        } => {
            let (phone, phone_from_config) = resolve_phone(
                phone,
                config.phone.as_deref(),
                otp.is_some() || cancel || poll,
            );
            let opts = LoginArgs {
                cdp: cdp_base(cdp.as_deref(), &config),
                phone,
                phone_from_config,
                channel: channel
                    .as_deref()
                    .map(login::Channel::parse)
                    .transpose()?
                    .unwrap_or(login::Channel::Sms),
                dry_run,
                otp,
                otp_prompt,
                poll,
                timeout: std::time::Duration::from_secs(timeout.unwrap_or(300)),
                cancel,
            };
            run_login(out, opts).await
        }
        AuthCmd::Import {
            cdp,
            from,
            address_only,
            ..
        } => match (from, address_only) {
            (Some(_), true) => Err(Error::invalid(
                "--address-only copies from the browser; it can't be combined with --from",
            )),
            (Some(path), false) => import_from(out, path),
            (None, true) => import_address(out, &cdp_base(cdp.as_deref(), &config)).await,
            (None, false) => import_cdp(out, cdp_base(cdp.as_deref(), &config)).await,
        },
        AuthCmd::Refresh { .. } => {
            let mut ctx = auth_context().await?;
            ctx.refresh_session().await?;
            let notes = ctx.after_login().await;
            print_refreshed(out, &ctx, "refreshed", &notes);
            Ok(())
        }
        AuthCmd::Hydra { .. } => {
            // Minting goes through Context so native/CDP policy applies.
            let mut ctx = auth_context().await?;
            let tok = ctx.mint_hydra().await?;
            let exp = grabcli_core::jwt::exp(&tok);
            if out.is_json() {
                out.json(&json!({"hydra_minted": true, "hydra_exp": exp}));
            } else {
                out.line(format!(
                    "minted hydra JWT (exp {})",
                    exp.map(|e| e.to_string()).unwrap_or_else(|| "?".into())
                ));
            }
            Ok(())
        }
        AuthCmd::Status { .. } => {
            let ctx = auth_context().await?;
            let mut status = ctx.auth_status();
            status["session_path"] =
                json!(FileSessionStore::default_path()?.path.display().to_string());
            let now = grabcli_core::jwt::now();
            if let Some(p) = local::read_pending()? {
                status["login_pending"] = p.summary(now);
            }
            let log = FileSendBudget::default_path()?.log()?;
            status["login_sends_last_hour"] = json!(log.sends_last_hour(now));
            status["login_next_send_in_secs"] = json!(log.wait_secs(now));
            if out.is_json() {
                out.json(&status);
            } else {
                print_status(out, &status);
            }
            Ok(())
        }
    }
}

async fn import_cdp(out: Output, base: String) -> Result<()> {
    // Preserve an existing hydra token if we already have a session.
    let store = FileSessionStore::default_path()?;
    let mut session = store.read().ok().flatten().unwrap_or_default();
    cdp::export_cookies(&base, &mut session).await?;
    store.write(&session)?;
    let logged_in = session.cookie("gfc_session").is_some();
    if out.is_json() {
        out.json(&json!({
            "imported": true,
            "cookies": session.cookies.len(),
            "source": "cdp",
            "logged_in": logged_in,
        }));
    } else {
        out.line(format!(
            "imported {} cookies from {base}",
            session.cookies.len()
        ));
        if logged_in {
            out.line("next: `grab auth hydra` (or just run a command; hydra auto-mints)");
        } else {
            out.line(
                "warning: no gfc_session cookie, so the browser is not logged in to food.grab.com. \
                 Log in there, then run `grab auth import` again.",
            );
        }
    }
    Ok(())
}

/// `grab auth import --address-only`: merge just the `location` cookie from
/// the browser's normal context into the saved session.
async fn import_address(out: Output, base: &str) -> Result<()> {
    let mut ctx = auth_context().await?;
    if !ctx.restore_address_from_browser(base).await? {
        return Err(Error::new(
            ErrorKind::AddressMissing,
            format!(
                "the browser at {base} has no saved delivery address (location cookie); pick an \
                 address on food.grab.com there, then run this again"
            ),
        ));
    }
    if out.is_json() {
        out.json(&json!({"imported": true, "source": "cdp", "address_only": true}));
    } else {
        out.line(format!(
            "copied the saved delivery address from {base} (other cookies unchanged)"
        ));
    }
    Ok(())
}

fn import_from(out: Output, path: std::path::PathBuf) -> Result<()> {
    let session = FileSessionStore { path: path.clone() }.require().map_err(|e| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("could not read {}: {}", path.display(), e.message),
        )
    })?;
    FileSessionStore::default_path()?.write(&session)?;
    if out.is_json() {
        out.json(&json!({"imported": true, "cookies": session.cookies.len(), "source": path.display().to_string()}));
    } else {
        out.line(format!(
            "imported {} cookies from {}",
            session.cookies.len(),
            path.display()
        ));
    }
    Ok(())
}

/// The phone a login uses: `--phone` wins, else config's `phone`, except for
/// the finishing/cancel modes (`--otp`, `--cancel`, `--poll` without
/// `--phone`), which never start a new login. Returns (phone, from_config).
fn resolve_phone(
    flag: Option<String>,
    config: Option<&str>,
    finishing: bool,
) -> (Option<String>, bool) {
    match (flag, config) {
        (Some(p), _) => (Some(p), false),
        (None, Some(p)) if !finishing => (Some(p.to_string()), true),
        _ => (None, false),
    }
}

struct LoginArgs {
    cdp: String,
    phone: Option<String>,
    phone_from_config: bool,
    channel: login::Channel,
    dry_run: bool,
    otp: Option<String>,
    otp_prompt: bool,
    poll: bool,
    timeout: std::time::Duration,
    cancel: bool,
}

fn print_refreshed(out: Output, ctx: &Context, kind: &str, notes: &[String]) {
    let status = ctx.auth_status();
    let secs = status
        .get("gfc_session_expires_in_secs")
        .and_then(serde_json::Value::as_i64);
    if out.is_json() {
        out.json(&json!({
            "kind": kind,
            "refreshed": kind == "refreshed",
            "gfc_session_exp": status.get("gfc_session_exp"),
            "gfc_session_expires_in_secs": secs,
            "notes": notes,
        }));
    } else {
        let what = if kind == "refreshed" {
            "session refreshed silently (no challenge sent)"
        } else {
            "already logged in"
        };
        match secs {
            Some(n) if n > 0 => out.line(format!("{what}; gfc_session valid ~{}m", n / 60)),
            _ => out.line(what),
        }
        for n in notes {
            out.line(format!("note: {n}"));
        }
    }
}

/// Silent path: already fresh, or refresh through the SSO cookie. `Ok(None)`
/// when there's no session to refresh; `Err` when the SSO is dead.
async fn try_silent(out: Output) -> Result<Option<&'static str>> {
    let Ok(mut ctx) = auth_context().await else {
        return Ok(None);
    };
    let fresh = ctx.session().gfc_logged_in()
        && ctx
            .session()
            .gfc_expires_at()
            .is_some_and(|e| e - grabcli_core::jwt::now() > 15 * 60);
    let kind = if fresh {
        "already_logged_in"
    } else {
        ctx.refresh_session().await?;
        "refreshed"
    };
    let notes = ctx.after_login().await;
    print_refreshed(out, &ctx, kind, &notes);
    Ok(Some(kind))
}

async fn login_ok(out: Output, session: &Session) {
    // Best-effort: fill in a missing address, mint hydra.
    let notes = match auth_context().await {
        Ok(mut ctx) => ctx.after_login().await,
        Err(_) => Vec::new(),
    };
    let now = grabcli_core::jwt::now();
    let secs = session.gfc_expires_at().map(|e| e - now);
    let sso = session
        .cookies
        .iter()
        .find(|c| c.name == grabcli_core::sso::SSO_COOKIE)
        .map(|c| c.expires as i64 - now);
    if out.is_json() {
        out.json(&json!({
            "kind": "login_ok",
            "cookies": session.cookies.len(),
            "gfc_session_exp": session.gfc_expires_at(),
            "gfc_session_expires_in_secs": secs,
            "sso_expires_in_secs": sso,
            "notes": notes,
        }));
    } else {
        out.line(format!(
            "logged in: saved {} cookies; gfc_session ~{}m, SSO cookie ~{}d (silent refresh works until then)",
            session.cookies.len(),
            secs.unwrap_or(0) / 60,
            sso.unwrap_or(0) / 86_400
        ));
        for n in &notes {
            out.line(format!("note: {n}"));
        }
        out.line("verify with `grab auth status`");
    }
}

fn pending_out(out: Output, p: &login::PendingLogin) {
    let now = grabcli_core::jwt::now();
    if out.is_json() {
        out.json(&login::pending_json(p, now));
        return;
    }
    let mins = (p.expires_at - now).max(0) / 60;
    match p.kind {
        login::PendingKind::OtpSent => out.line(format!(
            "sent one code to {} via {}. Finish within ~{mins}m: grab auth login --otp <code>",
            p.phone_masked,
            p.channel_sent.as_deref().unwrap_or("SMS/WhatsApp")
        )),
        login::PendingKind::ApprovalPending => out.line(format!(
            "sent one approval request to the Grab app for {}. Approve it, then: grab auth login --poll (~{mins}m left)",
            p.phone_masked
        )),
    }
}

fn read_stdin_code() -> Result<String> {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| Error::new(ErrorKind::Io, format!("reading the code from stdin: {e}")))?;
    Ok(line.trim().to_string())
}

async fn finish_otp(out: Output, code: &str) -> Result<()> {
    let code = if code == "-" {
        read_stdin_code()?
    } else {
        code.to_string()
    };
    let mut slot = local::read_pending()?;
    let res = login::finish_with_otp(&mut slot, &code, &previous_session()).await;
    local::write_pending(slot.as_ref())?;
    let session = res?;
    FileSessionStore::default_path()?.write(&session)?;
    login_ok(out, &session).await;
    Ok(())
}

async fn finish_wait(out: Output, timeout: std::time::Duration) -> Result<()> {
    let mut slot = local::read_pending()?;
    let res = login::wait_for_approval(&mut slot, timeout, &previous_session()).await;
    local::write_pending(slot.as_ref())?;
    match res? {
        login::WaitOutcome::LoggedIn(s) => {
            FileSessionStore::default_path()?.write(&s)?;
            login_ok(out, &s).await;
            Ok(())
        }
        login::WaitOutcome::StillWaiting => {
            if out.is_json() {
                if let Some(p) = &slot {
                    out.json(&login::pending_json(p, grabcli_core::jwt::now()));
                }
            } else {
                out.line("not approved yet; approve in the Grab app, then run `grab auth login --poll` again");
            }
            Ok(())
        }
    }
}

fn warn_tabs(out: Output, tabs: &[String]) {
    if let Some(w) = login::tracking_tab_warning(tabs) {
        out.warn(format!("warning: {w}"));
    }
}

async fn run_login(out: Output, a: LoginArgs) -> Result<()> {
    let modes = [a.otp.is_some(), a.cancel, a.phone.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    let with_phone = a.phone.is_some() && !a.dry_run;
    if modes > 1
        || (a.dry_run && a.phone.is_none())
        || (a.otp_prompt && !with_phone)
        || (a.dry_run && (a.poll || a.otp_prompt))
    {
        return Err(Error::invalid(
            "use one of: (no flags) | --phone N [--channel C] [--poll|--otp-prompt] | [--phone N] --dry-run | --otp CODE | --poll | --cancel",
        ));
    }
    if a.cancel {
        let mut slot = local::read_pending()?;
        let had = login::cancel(&mut slot).await;
        local::write_pending(None)?;
        if out.is_json() {
            out.json(&json!({"kind": "cancelled", "had_pending": had}));
        } else {
            out.line(if had {
                "cancelled the pending login and closed its isolated browser context (send limits unchanged)"
            } else {
                "no login was pending"
            });
        }
        return Ok(());
    }
    if let Some(code) = &a.otp {
        return finish_otp(out, code).await;
    }
    let Some(phone) = &a.phone else {
        if a.poll {
            return finish_wait(out, a.timeout).await;
        }
        // Bare `grab auth login`: silent only.
        return match try_silent(out).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(Error::new(
                ErrorKind::AuthMissing,
                "no saved session to refresh; start a login with `grab auth login --phone +63…` or set `phone` in config.toml (preview it with --dry-run first)",
            )),
            Err(e) => Err(e),
        };
    };

    let (cc, national) = login::split_phone(phone)?;
    let masked = login::mask_phone(&cc, &national);
    let tabs = login::shared_browser_tabs(&a.cdp).await;

    if a.dry_run {
        let now = grabcli_core::jwt::now();
        let log = FileSendBudget::default_path()?.log()?;
        let mut plan = login::dry_run_plan(&masked, a.channel, &a.cdp, &log, now);
        plan["phone_source"] = json!(if a.phone_from_config {
            "config"
        } else {
            "flag"
        });
        let reach = cdp::pages(&a.cdp).await;
        plan["browser_reachable"] = json!(reach.is_ok());
        plan["order_tracking_tabs"] = json!(tabs);
        if let Some(p) = local::read_pending()? {
            plan["pending"] = p.summary(now);
        }
        if out.is_json() {
            out.json(&plan);
        } else {
            out.line("dry run: nothing was sent.");
            out.line(format!(
                "without --dry-run this would log in {masked}{} on weblogin.grab.com through the browser at {}:",
                if a.phone_from_config { " (phone from config)" } else { "" },
                a.cdp
            ));
            if let Some(steps) = plan["steps"].as_array() {
                for (i, s) in steps.iter().enumerate() {
                    out.line(format!("  {}. {}", i + 1, s.as_str().unwrap_or_default()));
                }
            }
            out.line(format!("it sends: {}", a.channel.describe()));
            out.line(format!(
                "send limits: {} of {} used in the last hour; next send allowed {}",
                log.sends_last_hour(now),
                login::MAX_SENDS_PER_HOUR,
                match log.wait_secs(now) {
                    0 => "now".to_string(),
                    n => format!("in {n}s"),
                }
            ));
            match reach {
                Ok(_) => out.line("browser: reachable"),
                Err(e) => out.line(format!("browser: NOT reachable ({})", e.message)),
            }
            if let Some(p) = plan.get("pending") {
                out.line(format!("note: a login is already pending: {p}"));
            }
            out.line("to send, re-run the same command without --dry-run");
        }
        warn_tabs(out, &tabs);
        return Ok(());
    }

    // Silent first; only send when that can't work.
    match try_silent(out).await {
        Ok(Some(_)) => return Ok(()),
        Ok(None) => {}
        Err(e) if e.kind == ErrorKind::AuthExpired => {}
        Err(e) => return Err(e),
    }
    warn_tabs(out, &tabs);
    let mut slot = local::read_pending()?;
    let checked = login::check_no_pending(&mut slot, grabcli_core::jwt::now()).await;
    local::write_pending(slot.as_ref())?;
    checked?;
    let p = login::start(
        login::StartOpts {
            cdp_base: &a.cdp,
            phone,
            channel: a.channel,
            copy_browser_address: true,
        },
        &FileSendBudget::default_path()?,
    )
    .await?;
    local::write_pending(Some(&p))?;
    pending_out(out, &p);
    match p.kind {
        login::PendingKind::ApprovalPending if a.poll => finish_wait(out, a.timeout).await,
        login::PendingKind::OtpSent if a.otp_prompt => {
            if !out.is_json() {
                out.line("enter the code:");
            }
            finish_otp(out, "-").await
        }
        _ => Ok(()),
    }
}

fn print_status(out: Output, s: &serde_json::Value) {
    if let Some(p) = s.get("session_path").and_then(|v| v.as_str()) {
        out.line(format!("session: {p}"));
    }
    let now_note = |secs: Option<i64>| match secs {
        Some(n) if n > 0 => format!("in {}m", n / 60),
        Some(_) => "EXPIRED".to_string(),
        None => "session cookie".to_string(),
    };
    let gfc = match (
        s.get("gfc_session_present").and_then(|v| v.as_bool()),
        s.get("gfc_session_logged_in").and_then(|v| v.as_bool()),
    ) {
        (Some(false), _) => "missing (not logged in to food.grab.com)".to_string(),
        (_, Some(false)) => "anonymous (login started, not finished)".to_string(),
        _ => match s
            .get("gfc_session_expires_in_secs")
            .and_then(|v| v.as_i64())
        {
            Some(n) if n > 0 => format!("in {}m", n / 60),
            Some(_) => "EXPIRED".to_string(),
            None => "present (no expiry claim)".to_string(),
        },
    };
    out.line(format!("gfc_session: {gfc}"));
    out.line(format!(
        "hydra: {} ({})",
        if s.get("hydra_valid")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            "valid"
        } else {
            "missing/expired"
        },
        match s.get("hydra_expires_in_secs").and_then(|v| v.as_i64()) {
            Some(n) if n > 0 => format!("in {}m", n / 60),
            _ => "minted automatically when a command needs it".to_string(),
        }
    ));
    out.line(format!(
        "saved address: {}",
        if s.get("has_saved_address")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            "yes"
        } else {
            "no"
        }
    ));
    if let Some(d) = s.get("address_detail").and_then(|v| v.as_str()) {
        out.line(format!("unit: {d}"));
    }
    if let Some(n) = s.get("note_to_driver").and_then(|v| v.as_str()) {
        out.line(format!("driver note: {n}"));
    }
    out.line(format!(
        "payment default: {}",
        if s.get("payment_type_id_present")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            "set"
        } else {
            "none"
        }
    ));
    if let Some(p) = s.get("login_pending") {
        out.line(format!(
            "login pending: {} for {} ({}s left{})",
            p["kind"].as_str().unwrap_or("?"),
            p["phone"].as_str().unwrap_or("?"),
            p["expires_in_secs"].as_i64().unwrap_or(0),
            if p["expired"].as_bool().unwrap_or(false) {
                ", expired"
            } else {
                ""
            }
        ));
    }
    out.line(format!(
        "login sends: {} in the last hour; next allowed in {}s",
        s["login_sends_last_hour"].as_i64().unwrap_or(0),
        s["login_next_send_in_secs"].as_i64().unwrap_or(0)
    ));
    if let Some(cookies) = s.get("cookies").and_then(|v| v.as_array()) {
        out.line("cookies:");
        for c in cookies {
            let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let secs = c.get("expires_in_secs").and_then(|v| v.as_i64());
            out.line(format!("  {name}: {}", now_note(secs)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_phone;

    const FLAG: &str = "+63 900 000 0012";
    const CONF: &str = "+63 900 000 0034";

    #[test]
    fn flag_beats_config() {
        assert_eq!(
            resolve_phone(Some(FLAG.into()), Some(CONF), false),
            (Some(FLAG.into()), false)
        );
        assert_eq!(
            resolve_phone(Some(FLAG.into()), Some(CONF), true),
            (Some(FLAG.into()), false)
        );
    }

    #[test]
    fn config_fills_in_when_starting() {
        assert_eq!(
            resolve_phone(None, Some(CONF), false),
            (Some(CONF.into()), true)
        );
        assert_eq!(resolve_phone(None, None, false), (None, false));
    }

    #[test]
    fn config_never_starts_a_login_when_finishing() {
        assert_eq!(resolve_phone(None, Some(CONF), true), (None, false));
    }
}
