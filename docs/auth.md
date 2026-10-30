# Authentication

`grab` talks to GrabFood with the same credentials a logged-in browser uses.
There are three layers:

1. **Long-lived account cookies** (set at login, scoped to `.grab.com`):
   - `grabid-openid-authn-ck`: the GrabID single-sign-on (SSO) cookie. It
     lasts up to `remember_me_days` = 60 days when the login ticked "Remember
     me". This is the credential silent refresh uses. Its JWT `exp` is not
     authoritative: the IdP can revoke the session earlier (a logout anywhere,
     or a login made without "remember me").
   - `passenger_authn_token` (+ `_jti`): ~30 days. It is *not* the SSO
     credential for food.
2. **Short-lived web session** (scoped to `food.grab.com`):
   - `gfc_session`: 2 hours (`Max-Age=7200`). A server-signed JWT whose
     `sessionKey` claim is sent as `x-gfc-session` on foodweb API calls and
     whose `accessToken` claim is the bearer token. It holds **no refresh
     token**. Before login, food sets an *anonymous* `gfc_session` that only
     holds the PKCE verifier/state.
3. **Device token** (`hydra`): minted per request, see [hydra.md](hydra.md).

Background and the evidence behind all of this: [login.md](login.md).

## Silent refresh (`grab auth refresh`)

`POST /auth/access-token` is **not** a refresh endpoint. It only echoes the
access token of a live `gfc_session` and answers `401 Authentication
unavailable` otherwise (login.md §1). The real silent path is the browser's
OAuth round trip, done over plain HTTP in one cookie jar:

```
GET food.grab.com/auth/login                → 302 partner-api…/oauth2/authorize
                                              + anonymous gfc_session (PKCE)
GET …/oauth2/authorize   (SSO cookie sent)  → 302 food.grab.com/auth/callback?code&state
                                              or weblogin …&auth_cookie_exists=true
                                                 → GET authorize + user_selection_done=true&remember_me=true
                                                   ("Continue as …", a plain navigation, no captcha)
                                              or weblogin without auth_cookie_exists → SSO dead
GET food.grab.com/auth/callback?code&state  → food exchanges the code (PKCE) and
                                              sets the logged-in gfc_session
```

Each hop is checked against a fixed allowlist (`food.grab.com/auth/login`,
`/auth/callback`, `{partner-api,api}.grab.com/grabid/v1/oauth2/authorize`) and
redirects are never followed blindly. This path cannot trigger an OTP, a push
or a captcha. If the SSO session is dead, it fails with `auth_expired` and the
message says to run `grab auth login`. The session file is only replaced on
success.

**Auto-refresh** (`auto_refresh`, default on): before a call, if the logged-in
`gfc_session` has under 15 minutes left, the client refreshes once,
best-effort. After a `401`, it refreshes once and retries the request once.
Order submission (`no_refresh`) is never retried. A missing or anonymous
`gfc_session` is left to the 401 path, so commands that need no login don't pay
for a refresh.

## Full login (`grab auth login`)

When the SSO cookie is dead, a real login is needed. grabcli does **not**
replay or forge the login page's anti-bot tokens (reCAPTCHA Enterprise, the
`x-hydraweb-jwt` device fingerprint). Instead it drives the **real
weblogin.grab.com page** over CDP in an **isolated browser context**
(`Target.createBrowserContext`). The context has its own cookie jar and shares
nothing with the user's tabs, so no other tab can log it out. The page does its
own reCAPTCHA and fingerprinting, the way it would for a person.

```
grab auth login                                   # silent refresh; never sends unless
                                                  # `phone` is set in config.toml (below)
grab auth login --phone +63XXXXXXXXXX [--channel app|sms|whatsapp] --dry-run
                                                  # prints the plan, sends nothing
grab auth login --phone +63XXXXXXXXXX [--channel app|sms|whatsapp]
                                                  # sends ONE challenge, then exits
grab auth login --otp 123456                      # finish with the code (or --otp - for stdin)
grab auth login --poll [--timeout 300]            # finish after approving in the Grab app
grab auth login --cancel                          # drop the pending login, close its context
# single process instead of two:
grab auth login --phone … --channel app --poll
grab auth login --phone … --channel sms --otp-prompt   # reads the code from stdin
```

**Saved phone.** With `phone = "+63XXXXXXXXXX"` in `config.toml`, a login
that would start a new attempt (bare `grab auth login`, `--dry-run`,
`--channel …`, `--otp-prompt`) uses that number exactly as if `--phone` had
been passed: same silent-refresh-first, send limits, isolated context and JSON
kinds. A still-valid session still returns `already_logged_in` and sends
nothing. `--phone` overrides it. The finishing modes (`--otp`, `--cancel`, and
`--poll` without `--phone`) never pick it up, so they can't start a new send;
the one-process `--channel app --poll` form therefore still needs an explicit
`--phone`. The number is validated when the config loads (an `invalid_input`
error that never echoes it) and is only ever shown masked; `--dry-run` reports
`"phone_source": "config"` or `"flag"`.

What `--phone` does, step by step:

1. Runs the silent refresh first. If it works (or the session is still
   fresh), it stops and sends nothing.
2. Checks the send budget and refuses with `rate_limited` if needed.
3. Creates an isolated context with `disposeOnDetach: false` and opens a
   background tab on `food.grab.com/auth/login`. The tab lands on
   `weblogin.grab.com/passenger/login` with food's PKCE state. Focus emulation
   is turned on so the SPA renders in a background tab. A hidden tab never runs
   `requestAnimationFrame`, so without it the page stays on its spinner.
4. Checks that the page shows the right country code, keeps "Remember me on
   this device for 60 days" ticked (the 60-day SSO), types the number with
   real key events, and checks that the field holds exactly those digits.
5. **Records the send** in the budget file, then clicks Continue. If Grab
   shows "Choose a preferred way to sign in", it clicks the **one** option
   that matches `--channel`: `app` → `PUSH_V2`/`PUSH`; `sms`/`whatsapp` →
   `CHANNEL_OPTIMIZED_PHONE_OTP`/`SMS_OTP`. Grab may already dispatch a
   challenge on Continue. In that case nothing else is clicked.
6. Reads the resulting screen: code entry ("Check your SMS/WhatsApp") or
   "Approve in mobile app". It saves the pending state and exits. The context
   and tab stay alive in the browser.

Finishing (`--otp` / `--poll`) reattaches to the same tab. For a code, it types
it into the page, which submits by itself. For approval, it watches the page.
Then the page redirects through `authorize` → `food.grab.com/auth/callback`
inside the context. grabcli reads that context's grab.com cookies with
`Storage.getCookies {browserContextId}` and waits for a logged-in
`gfc_session`. It saves them as the session (hydra, payment and address
fields are kept) and disposes the context.

**Channels.** For OTP, Grab picks the delivery (SMS or WhatsApp) on the first
send. The page offers other channels only behind "Resend code", which grabcli
never clicks. The channel actually used is reported (`channel_sent`).

**Why across processes works.** Contexts created with `disposeOnDetach: false`
outlive the CDP connection: verified on Chrome 155, both the context and its
tab are still there after the creating websocket closes. The in-page state
(the challenge session) lives in the still-running tab. If the browser restarts
or the tab is closed, `--otp`/`--poll` report `login_expired`. Start again
(within the send budget). The one-process forms (`--phone … --poll`,
`--phone … --otp-prompt`) are there for callers that can relay input on stdin.

### Safety rails

- Only a login with a phone (`--phone`, or config `phone` for bare `grab auth
  login`) and without `--dry-run` sends anything. Without a config `phone`,
  bare `grab auth login` never sends; `grab auth refresh` never sends at all.
  `--dry-run` makes no Grab calls. It only lists the browser's tabs (`GET /json`) to warn
  about order-tracking tabs. `/json` can list a just-closed tab for a moment,
  so a hit is re-checked once ~1.5 s later and only tabs seen both times are
  reported.
- At most **one** challenge per `--phone`, never retried, never resent. The
  send is recorded *before* the click, so a crash still counts.
- **60 s cooldown and at most 3 sends per rolling hour**, stored in
  `$STATE/login-sends.json` (0600). `--cancel` does not reset it.
- Pending state goes in `$STATE/login-pending.json` (0600, atomic). It holds
  the CDP endpoint, the context and tab ids, the requested and actual channel,
  timestamps (10-minute expiry) and the **masked** phone (`+63••••••••12`).
  No cookies, tokens, OTPs or full phone number are stored there or printed.
- Other tabs are never attached to, navigated or closed. If the browser has
  food.grab.com order-tracking tabs (`/order/…`, `/track`), `grab auth login`
  warns about them (see below).
- Step-ups that need a person (reCAPTCHA v2 checkbox, selfie, PIN, Arkose)
  end the attempt with `captcha_required`. In that case, log in at
  food.grab.com in a normal window and run `grab auth import`.

### Order-tracking tabs log the browser out

A food.grab.com **Track Order** tab (`/ph/en/order/<id>`) that is still open
after the browser's food session lapses polls `/track`, gets `401`, and then
calls GrabID `oauth2/logout` and food `/auth/logout`, roughly every 16 s.
That kills any login made in the browser's normal profile within seconds. It
is why `gfc_session` "never stuck" in the shared browser. The isolated login
context is not affected, but such tabs still break `grab auth import` and
normal browsing. Close them. grabcli only warns.

### `--json` kinds

Every stdout document has a `kind`: `already_logged_in`, `refreshed`,
`otp_sent`, `approval_pending`, `login_ok`, `cancelled`, `dry_run`.

```jsonc
{"kind": "otp_sent", "phone": "+63••••••••12", "channel_sent": "SMS", "expires_at": 1790000600, "expires_in_secs": 600, "next": "grab auth login --otp <code>", …}
{"kind": "approval_pending", "phone": "+63••••••••12", "channel_sent": "app", "expires_at": 1790000600, "next": "grab auth login --poll", …}
{"kind": "login_ok", "cookies": 23, "gfc_session_exp": 1790007200, "gfc_session_expires_in_secs": 7200, "sso_expires_in_secs": 5184000, "notes": []}
```

A `--poll` that times out while the app still hasn't been approved prints
`approval_pending` again (exit 0); run it again. Errors: `otp_invalid` (the pending
login is kept, so retry `--otp`), `captcha_required`, `login_expired`,
`rate_limited` (with `details.retry_after_secs`), `auth_expired`,
`cdp_unavailable`.

### Verified live

On 2026-10-30, against a real PH account, in a separate headed Chrome (fresh
profile, its own `--remote-debugging-port`, under a virtual display; headless
gets a CloudFront 403):

- `--phone` sent one challenge. Grab skipped the "choose a way" screen and
  went straight to **in-app approval** (`approval_pending`). Once it was
  approved in the Grab app, `--poll` returned `login_ok` with 23 cookies,
  including `passenger_authn_token(_jti)`, `grabid-openid-authn-ck` (60 days)
  and `gfc_session`. The login tab was closed, the pending file deleted, and
  the browser's other tabs were left alone. The new session served
  `grab food orders --ongoing` and a silent refresh.
- That run used an earlier driver that opened a plain new tab. A
  `background: true` tab without focus emulation is `visibilityState: hidden`
  and the login SPA never renders `#Phone`. The current driver (isolated
  context, background tab, `Emulation.setFocusEmulationEnabled` +
  `Page.setWebLifecycleState active`) was re-checked in the same browser up to
  the phone form, without sending: `visible`, phone form after ~2 s, `+63` and
  "Remember me" present. The context was disposed and other tabs untouched.
- The live `gfc_session` JWT has no `exp` claim (`nonce`, `state`,
  `codeVerifier`, `accessToken`, `sessionKey`, …). Its lifetime is the cookie's
  `Max-Age=7200`, which `gfc_expires_at` uses.
- Not yet seen live: the OTP screen (code entry, wrong code), the challenge
  picker and captcha step-ups. Their handling comes from [login.md](login.md)
  §2 and the weblogin bundle.

## Importing instead (`grab auth import`)

`grab auth import --cdp URL` copies the cookies of a browser that is already
logged in at food.grab.com, and `--from PATH` copies an existing
`session.json`. Neither creates credentials.

## The saved delivery address

food.grab.com keeps the delivery address in a `location` cookie (URL-encoded
JSON: id, lat/lng, address). The isolated login context doesn't have one, so
when a login saves the session it carries the preference cookies `location`,
`gfc_country` and `next-i18next` over from the previous session file (never
any auth cookie, and never overwriting what the login set). If there is still
no address, it reads `location` from the browser's normal (default) context
over CDP (`Storage.getCookies` without a `browserContextId`). `grab auth
refresh` keeps existing cookies and fills in a missing address the same way.

`grab auth import --address-only [--cdp URL]` re-syncs just that cookie from
the browser's normal context, leaving the login cookies untouched. Use it
after changing the address in the browser. Commands that need an address fail
with `address_missing` (exit 2) when there is none. Values are kept in memory
and in the 0600 session file only; nothing prints them.

After a login or refresh, grabcli also mints the hydra JWT once (best-effort;
it lives ~10 minutes, and every command that needs it mints it on demand), and
tries the silent GrabMart login (mart.grab.com has its own OAuth client; see
docs/mart.md). If Grab shows a one-time consent screen for GrabMart, grabcli
stops and asks you to approve it in the browser; it never approves consent
itself.

`GRABCLI_TRACE_SSO=1` prints each hop of the silent round trips to stderr as
host, path and query parameter *names* only (no values, no cookies).
