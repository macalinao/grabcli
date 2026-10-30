# Login and session refresh: findings and design

Research written on 2026-10-17 and updated the same day once the work was
implemented. No OTP was requested, no push approval was triggered, and no
login form was submitted while this was written. Values of cookies, tokens and
the account phone number are deliberately left out. Only names, lifetimes and
statuses appear here.

**What shipped** (see [auth.md](auth.md) for the user-facing description):

- `grab auth refresh` runs the silent SSO round trip from §1, "Refresh fixes"
  item 1. The `/auth/access-token` call is gone.
- `grab auth login` uses the **page-driving** option from §3, the fallback,
  not the header-only hybrid. It drives the real weblogin page in an isolated
  CDP browser context and never replays or forges `X-Recaptcha` or
  `x-hydraweb-jwt`. §3 and §4 below are kept as research. The "single request"
  in §4 is **not** what grabcli sends.
- §5 below records why the shared browser kept getting logged out.

## 1. Why `grab auth refresh` fails

### Observed

`grab auth status` (session exported from the CDP browser at 22:38 local):

| cookie | domain | cookie expiry | notes |
|---|---|---|---|
| `gfc_session` | food.grab.com | **missing** | status prints `gfc_session: ?` |
| `gfc_session_guid` | food.grab.com | session cookie | anonymous id only |
| `passenger_authn_token` (+`_jti`) | .grab.com | 30 d | JWT, `aud=SSO_TOKEN_ISSUING_SERVICE`, `amr=WEBLOGIN`, re-issued within the last hour |
| `grabid-openid-authn-ck` | .grab.com | 58 d | JWT, `aud=SSO_TOKEN_ISSUING_SERVICE`, `amr=WEBLOGIN`, issued 2 d ago, JWT `exp` +58 d |
| `hwuuid`/`hwuuidtime`, `_gsvid`, `aws-waf-token`, analytics | various | 4 to 400 d | device and analytics cookies only |

The session file and the browser have no cookie named `pid`. The browser
itself is logged out of food.grab.com: it has no `gfc_session`, and its
weblogin tab sits on the "Welcome back… Continue" saved-account screen.

`grab auth refresh` returns `auth_expired` (exit 4). The underlying request is:

```
POST https://food.grab.com/auth/access-token   (X-Grab-Request: 1, body {})
→ 401 application/json  {"message":"Authentication unavailable"}
  Set-Cookie: next-i18next, gfc_country   (no gfc_session)
```

The response is identical whether only food.grab.com cookies or every stored
cookie (including `passenger_authn_token` and `grabid-openid-authn-ck`) is sent.

### Root cause 1: `/auth/access-token` is not a refresh endpoint

The `gfc_session` cookie is a server-signed JWT whose payload holds
`nonce, state, codeVerifier, accessToken, successRedirect, failureRedirect,
name, safeId, sessionKey, newUser`. It has **no refresh token**.
`/auth/access-token` (`getSessionAccessToken` in the food bundle) only hands
back the `accessToken` stored in that cookie. With no logged-in `gfc_session`
it has nothing to return, so it answers `401 Authentication unavailable`. It
never reads the `.grab.com` cookies. The premise in auth.md ("authorised
using the long-lived `.grab.com` cookies") is wrong. Commit 5f34b75 never
tested it against a live session, as the 2026-10-15 vault note says.

`gfc_session` is set with `Max-Age=7200`. Once it is gone, the only way back
is the OAuth authorize round-trip.

### Root cause 2: the SSO session behind `grabid-openid-authn-ck` is dead server-side

The real silent path is the one the browser takes: `food.grab.com/auth/login`
→ `partner-api.grab.com/grabid/v1/oauth2/authorize` → (valid SSO) → code →
`food.grab.com/auth/callback`. Probed read-only (no redirects followed, no
code exchanged):

```
GET food.grab.com/auth/login             → 302 authorize  (sets anonymous PKCE gfc_session)
GET partner-api…/oauth2/authorize        → 302 weblogin.grab.com/passenger/login?…
                                           (no auth_cookie_exists param → IdP did not accept the SSO cookie)
GET api.grab.com/grabid/v1/oauth2/user_login_info
    with grabid-openid-authn-ck          → 500 {"errors":[{"message":"no data in result set","details":{"func":"UserLoginInfo"}}]}
    without it / only passenger_authn_*  → 400 code 10015 "Required parameters are either missing or is incorrect"
```

So the IdP parses `grabid-openid-authn-ck`, but has no record for it. Its
JWT `exp` (+58 d) is **not authoritative**: the server-side SSO session was
revoked or never persisted. `passenger_authn_token` is a separate token that
the IdP's authorize and `user_login_info` do not use as the SSO credential.

Possible reasons the SSO record vanished (cannot be distinguished from
outside):

- A logout somewhere: food `GET /auth/logout` calls grabid `POST /v1/oauth2/logout`.
  "Remove an account" on weblogin calls `/grabid/v1/authnv4/logout`.
- A login done with `ssoValidityInSecs = 0`. The weblogin SPA computes
  `ssoValidity = rememberMe ? rememberMeDays*86400 : 0`. `rememberMeDays`
  defaults to 0 and is only filled from `weblogin-config` (60) by the
  phone-login page chunk. The saved-account "Continue" page does not load that
  chunk, so a "Continue" login can plausibly ask for a 0-second SSO. This fits
  the 2026-10-15 note that "saved-account Continue loops to login". This is a
  hypothesis.

### Conclusion

- Once `gfc_session` (2 h) is gone, **silent refresh is only possible through
  the SSO cookie**, and only while the IdP still has its server-side record.
  Today it does not, so **a full login is required**. No combination of the
  stored cookies can recover the session.
- Real lifetimes. `gfc_session` is 2 h (`Max-Age=7200`).
  `grabid-openid-authn-ck` nominally lasts `remember_me_days` = 60 d, but the
  server can revoke it earlier; check liveness with `user_login_info`.
  `passenger_authn_token` lasts 30 d. It is not the SSO credential for food.
  The weblogin saved-account ID token (localStorage
  `passenger_saved_accounts`, `aud=PASSENGER`) lasts 90 d. It is a login
  *hint* and starts a new login, usually with a challenge. It is not a session.

### Refresh fixes (1, 3 and 5 are implemented; 2 dropped; 4 open)

1. **Implement real silent refresh via SSO.** Replace the `/auth/access-token`
   POST with:
   1. `GET https://food.grab.com/auth/login` (no follow). Keep the new
      anonymous `gfc_session`, which holds the PKCE verifier and state, and
      the `Location`.
   2. `GET` that authorize URL with the `.grab.com` cookies (no follow).
      - If it goes to `food.grab.com/auth/callback?code=…&state=…`, follow it
        with the PKCE `gfc_session`. The response sets the logged-in
        `gfc_session`. Done.
      - If it goes to `weblogin…/passenger/login?…&auth_cookie_exists=true…`,
        re-issue authorize with the same OAuth params plus
        `user_selection_done=true&remember_me=true`. This is exactly what the
        "Continue as …" button does, as a plain top-level navigation with no
        recaptcha. Then follow to the callback.
      - Anything else (weblogin without `auth_cookie_exists`) means
        `auth_expired: login required`.

   The allowlist needs `food.grab.com/auth/login`,
   `food.grab.com/auth/callback`, and
   `partner-api.grab.com/grabid/v1/oauth2/authorize`. The callback branch is
   inferred from the SPA code. It has not been observed, because no live SSO
   existed to test with.
2. **Keep `/auth/access-token` only as a cheap probe.** It does not extend
   anything. If it is kept at all, read the 401 body's `message`. That message
   is not a credential, and it lets the client report "no web session"
   instead of a generic expiry.
3. **Refresh proactively.** Run the SSO refresh when `gfc_session` is missing
   or has less than about 15 minutes left. Do not wait for the 401. Do it
   before order submission as well (already the policy).
4. **Liveness check for the long-lived cookie.** Add
   `grab auth status --check`, which does
   `GET api.grab.com/grabid/v1/oauth2/user_login_info`. A 200 means the SSO
   is alive. A 500 "no data in result set" means it is dead. Never print the
   response: it contains the account name and anonymized phone. Report
   `sso: alive|dead` and label `grabid-openid-authn-ck` as the SSO cookie.
5. **Small correctness fixes in the current code:**
   - `print_status` prints `?` for a missing `gfc_session`. It should print
     `missing`.
   - `parse_set_cookie` ignores `Expires` and only reads `Max-Age`.
   - It strips the leading dot from `Domain`, so `upsert_cookie`, which
     matches on (name, domain), creates a duplicate for `.grab.com` cookies
     instead of replacing them.
   - `Session::cookie_header()` sends every stored cookie to every host,
     including duplicate `_gsvid`, `hwuuid` and `_dd_s` from
     weblogin/mart/paysdk. Filter by domain match per request host.

   All of these are unit-testable without touching login endpoints.

## 2. How the food.grab.com web login works

### OAuth client

`GET https://food.grab.com/auth/login` → 302 →
`https://partner-api.grab.com/grabid/v1/oauth2/authorize` with:

| param | value |
|---|---|
| `client_id` | `4ddf78ade8324462988fec5bfc5874c2` (public, "GrabFood") |
| `response_type` | `code` |
| `redirect_uri` | `https://food.grab.com/auth/callback` |
| `scope` | `openid profile.read foodweb.order foodweb.rewards foodweb.get_enterprise_profile food-web.messagecenter paysdk.pm.list` |
| `acr_values` | `service:PASSENGER consent_ctx:countryCode=PH` |
| `code_challenge_method` | `S256`; challenge, `state` and `nonce` generated by food's server, with the verifier kept in `gfc_session` |

authorize → `https://weblogin.grab.com/passenger/login?` with the OAuth params
plus `ctx_id, request_id, service_id=PASSENGER, login_method=pax, gw=pgw,
auth_endpoint, forwardedHost, browser, device_type`, and
`auth_cookie_exists=true` when it sees a valid SSO. With `gw=pgw`, the SPA
sends every API call to `https://partner-api.grab.com` (otherwise
`api.grab.com`), with `withCredentials`.

Public client config (`GET /grabid/v1/oauth2/clients/{client_id}/weblogin-config`):
`new_login_api_enabled: true`, `remember_me_days: 60`,
`sso_validity_seconds: 30`, `is_otp_require_recaptcha: true`,
`is_google_enabled / is_facebook_enabled / is_apple_enabled / is_passkey_enabled: true`,
`arkose_enabled: false`, `is_rba_enabled: false`,
`registration_not_allowed_in_countries: ["PH"]`.

### Login methods offered (weblogin build WEBLOGIN-2026-10-15)

- **Phone number** (`accountIdentifierType: PHONE`, value = country code +
  number, digits only). Then a challenge:
  - `CHANNEL_OPTIMIZED_PHONE_OTP` with `channel` ∈ `SMS | WHATSAPP | VIBER | ZALO`
    (`SMS_OTP` is the legacy type).
  - `PUSH_V2` / `PUSH`: approve in the Grab app. The page polls the challenge
    session every 1 s and can show a QR code.
  - `PIN`, `PASSKEY`, `EMAIL_OTP`, `WEB_SELFIE`, and `RECAPTCHA` (v2
    checkbox) or `ARKOSE` (disabled for this client) as risk step-ups.
- **Saved account** ("Welcome back… Continue"):
  `accountIdentifierType: IDTOKEN` with the 90-day saved-account token from
  localStorage. This is a fresh login that usually still requires a challenge.
- **Google** (One Tap or button, `GOOGLE_ID_TOKEN`), **Facebook**
  (`FACEBOOK_ACCESS_TOKEN`), **Apple** (`APPLE_ID_TOKEN`), and **passkey**
  (WebAuthn, `rpId grab.com`). All of these need a real browser or the user's
  IdP session.
- Email or username plus password exists in the bundle for
  enterprise/partner clients (`/oauth2/v2/risk_login`, `/login/method`). It
  does not apply to the pax client.

### Endpoint sequence: new (v4 "challenge session") API, used because `new_login_api_enabled=true`

All requests go to `https://partner-api.grab.com`. Every call carries the
headers `x-hydraweb-jwt`, `x-tracking-id` and `x-accept-language`. Calls marked
(R) also carry `X-Recaptcha`.

1. **Start login (R)**: `POST /grabid/v1/authnv4/login`

   ```json
   {"challengeSessionID":"", "accountIdentifier":"63XXXXXXXXXX",
    "accountIdentifierType":"PHONE", "serviceID":"PASSENGER",
    "redirect":"<authorize URL + user_selection_done=true&remember_me=true>",
    "metadataType":"PAX_WEBLOGIN",
    "metadata":{"ctxId":"<ctx_id>","clientId":"<client_id>","jwtTxID":"<request, if present>",
                "ssoValidityInSecs":"5184000"}}
   ```

   The expected answer is the error
   `ERROR_CODE_CHALLENGE_SESSION_COMPLETION_REQUIRED`, with
   `details.challengeSessionID`, `details.currentChallenges` and
   `details.currentGeneratedChallenge`. **If `currentGeneratedChallenge` is
   set, the server already dispatched the first challenge (SMS or push) on
   this call.**
2. **Send challenge (R)**:
   `POST /grabid/v1/challengesession/challengeSession/generateChallenge`
   with `{challengeSessionID, challengeType:"CHANNEL_OPTIMIZED_PHONE_OTP",
   payload:{paxWebLoginCtx:{clientID, ctxID, jwtTxID}, channel:"SMS"}}`.
   This sends the OTP (or the push, for `PUSH_V2`).
3. **Push only:** poll
   `GET /grabid/v1/challengesession/challengeSession?challengeSessionID=…&isIncludeCurrentGeneratedChallenge=true`
   about every 1 s until `isCompleted`.
4. **Verify OTP (R)**:
   `POST /grabid/v1/challengesession/challengeSession/verifyChallenge`
   with `{challengeSessionID, challengeType, payload:{code:"123456", accountIdentifierType:"PHONE", status}}`.
5. **Finish login**: `POST /grabid/v1/authnv4/login`, the same body with
   `challengeSessionID` filled in. No recaptcha is attached when a session ID
   is present. The response is `{redirect, displayToken}`, plus `Set-Cookie`
   for the SSO cookies on `.grab.com`.
6. **Authorize**: navigate to `redirect` (authorize +
   `user_selection_done=true&remember_me=true`). The response is a 302 to
   `food.grab.com/auth/callback?code&state`.
7. **Callback**: `GET food.grab.com/auth/callback?code&state` with the PKCE
   `gfc_session`. Food exchanges the code server-side and sets the logged-in
   `gfc_session`.

Errors carry `details.remainingTimeInSec` for blocks. Codes include
`ERROR_CODE_EXCEEDED_MAX_ATTEMPTS`, `…NOT_AMONG_ACCEPTED_CHALLENGES` and
`…USER_ACCOUNT_BLOCKED`.

The legacy API is still in the bundle: `POST /oauth2/otp` (`X-HYDRAWEB-JWT`,
`X-Recaptcha`) → `/oauth2/verify-phone` → `/oauth2/login {ctx_id, token,
sso_validity_seconds}` → authorize. It also has `/oauth2/asyncauth/status`
polling for the old push flow and QR login. auth.md describes this API, but
this client uses the v4 API.

### Anti-bot measures

- **reCAPTCHA Enterprise v3 (invisible, score-based).** Site key
  `6Lc_6OUe…` (in the weblogin config). Header `X-Recaptcha`. The action
  names are the endpoint paths: `/v1/authnv4/login`,
  `/v1/challengesession/challengeSession/generateChallenge`, and
  `/v1/challengesession/challengeSession/verifyChallenge` (legacy:
  `/v1/oauth2/otp`, `/v1/mfa/v2/*`). The site key is domain-bound to
  weblogin.grab.com. `is_otp_require_recaptcha: true`.
- **`x-hydraweb-jwt` device fingerprint.** RS256, signed with a key embedded
  (obfuscated) in `static/js/hydra.*.js`. Claims include `iss: gaa`,
  `aud: wlg`, `name: grabtaxi`, and `exp` = 600 s after issue. The fields come
  from browser probes: devtools open, WebAssembly, headless detection,
  incognito detection, mouse and keypress counts, battery, monitor, canvas and
  WebGL. It also includes a **Tongdun** device fingerprint
  (`idfp.tongdun.net`, partner `Grab_id`) and the `hwuuid` cookie and
  localStorage pair. The token could be forged natively because the key ships
  in the bundle, but that would mean faking anti-fraud signals. Don't.
- **Risk step-ups** returned by the server: RECAPTCHA v2 checkbox, ARKOSE
  (off for this client), WEB_SELFIE, PIN. These need a human.
- AWS WAF (`aws-waf-token`) covers `.food.grab.com` only. The read-only
  `partner-api`/`api.grab.com` GETs above worked from curl without it.

## 3. Native `grab auth login`: feasibility and design (research; superseded)

> Implemented differently: grabcli drives the page (the "fallback" below),
> because the decision was not to replay or forge anti-bot tokens from plain
> HTTP. The resulting flow is in [auth.md](auth.md#full-login-grab-auth-login).

**Feasible as a hybrid.** grabcli does all the HTTP itself, holds all cookies
and state, and does the PKCE/callback. The **CDP browser on
`weblogin.grab.com` mints only the two page-bound headers**:

- `X-Recaptcha`: `cdp::evaluate_in_origin(base, "https://weblogin.grab.com",
  "https://weblogin.grab.com/passenger/login", "grecaptcha.enterprise.execute('<site key>', {action})")`.
  This is the same pattern as `grabmart/src/recaptcha.rs`. Load
  `https://www.google.com/recaptcha/enterprise.js?render=<key>` if
  `grecaptcha.enterprise` is absent. Mint one token per call, never cache it,
  and never log it.
- `x-hydraweb-jwt`: sniff it from the page's own request, the same way as
  `cdp::mint_hydra`. Loading `/passenger/login?client_id=…` fires
  `GET …/weblogin-config` carrying `x-hydraweb-jwt`. Capture it via
  `Network.requestWillBeSent` and reuse it for ≤ 10 min. Use the browser's
  User-Agent for all requests.

Fallback if the risk engine dislikes a header-only hybrid (low score, a
RECAPTCHA or SELFIE challenge): drive the actual weblogin page over CDP.
Type the phone, click Next, then hand the OTP to the page. Read the result
from the callback cookies with `cdp::export_cookies`.

### CLI shape (resumable, assistant-relayable)

```
grab auth login --phone +63XXXXXXXXXX [--channel sms|whatsapp|push] [--json]
    → steps 0–2 below; prints otp_sent{channel, expires_in} or approval_pending
grab auth login --otp 123456 [--json]      → steps 4–7 → login_ok
grab auth login --poll [--json]            → step 3 loop → login_ok | approval_pending
grab auth login --cancel                   → delete pending state
```

- Step 0 is the silent attempt: run the SSO refresh (§1 fix 1) first. If that
  works, no OTP is ever sent.
- Pending state goes in `$STATE/login-pending.json`, written 0600 and
  atomically (`paths::write_private`). It holds the PKCE `gfc_session`,
  weblogin query params (`ctx_id`, `request_id`, `state`, `nonce`,
  `code_challenge`, `client_id`), `challengeSessionID`, `challengeType`, all
  interim cookies, and `created_at`/`expires_at`. Expire it at about 10 min,
  or the challenge's `expires_at`. Never store the phone number; keep only the
  last 2 digits for display.
- JSON kinds: `already_logged_in`, `refreshed`, `otp_sent`,
  `approval_pending`, `login_ok`, `otp_invalid` (with `attempts_left`),
  `captcha_required` (a v2, Arkose or selfie step-up; tell the user to finish
  in the browser, then `grab auth import`), `login_expired`, `blocked`
  (`retry_after_secs`).
- Safety rails:
  - Allowlist exactly these:
    - `partner-api.grab.com/grabid/v1/{authnv4/login, challengesession/challengeSession, …/generateChallenge, …/verifyChallenge, oauth2/authorize, oauth2/clients/*/weblogin-config}`
    - `food.grab.com/auth/{login,callback}`
  - **Never auto-retry** `authnv4/login` (phone) or `generateChallenge`. Each
    one can send an SMS.
  - Have a cooldown of at least 60 s between OTP sends. Have a hard cap of 3
    sends per hour, persisted in the state file.
  - Make `--phone` require an explicit `--yes-send-otp` (or an interactive
    confirm), mirroring the order-submit rule.
- After `login_ok`, check `user_login_info` (SSO alive) and the
  `gfc_session` `exp`. Persist the session the same way as import.

### What ends up in the session

The new logged-in `gfc_session` from the callback, the refreshed `.grab.com`
SSO cookies (`grabid-openid-authn-ck`, `passenger_authn_token*`) from step 5,
and the existing device cookies. Refresh then works via §1 fix 1 for up to
60 days, as long as the login sent `ssoValidityInSecs = 5184000` and authorize
had `remember_me=true`. Both are explicit in this design rather than relying
on SPA defaults.

### Risks

- Account risk: OTP floods or many failed attempts lead to
  `EXCEEDED_MAX_ATTEMPTS` or temporary blocks, and possibly an account flag.
  Hence the caps and the no-retry rule.
- Risk-engine mismatch: a header-only hybrid sends requests from reqwest, not
  from the page, so the TLS/JA3 fingerprint and request timing differ from
  Chrome's. That can lower the score and trigger a step-up. Mitigation: use
  the page-driving fallback.
- The hydraweb token carries automation signals. The CDP browser has
  `navigator.webdriver` and devtools attached, so it may score as "suspicious"
  in either mode.
- Bundle drift: endpoint paths, action names and payload fields are taken
  from minified JS (build `WEBLOGIN-2026-10-15-master-061420`) and can change
  silently.
- Unverified details: whether `authnv4/login` auto-dispatches the first
  challenge, the exact `channel` casing, and whether authorize with
  `user_selection_done=true` skips weblogin when SSO is valid. These need one
  live run.
- Terms of service: this automates a first-party login with the user's own
  credentials and challenge. No captcha is solved or bypassed. Tokens come from
  the real page in the user's own browser.

## 4. The single request for a live test (research only; not used)

One request, from grabcli (or curl), after a silent SSO attempt has failed:

```
POST https://partner-api.grab.com/grabid/v1/authnv4/login
Origin: https://weblogin.grab.com
Referer: https://weblogin.grab.com/passenger/login?<params from authorize>
User-Agent: <the CDP browser's UA>
Content-Type: application/json
x-accept-language: en
x-hydraweb-jwt: <sniffed from the weblogin page in the CDP browser>
X-Recaptcha: <grecaptcha.enterprise.execute(siteKey, {action:"/v1/authnv4/login"}) on weblogin.grab.com>
Cookie: <.grab.com + partner-api cookies>

{"challengeSessionID":"","accountIdentifier":"63<phone number>","accountIdentifierType":"PHONE",
 "serviceID":"PASSENGER","redirect":"<authorize URL + user_selection_done=true&remember_me=true>",
 "metadataType":"PAX_WEBLOGIN",
 "metadata":{"ctxId":"<ctx_id>","clientId":"4ddf78ade8324462988fec5bfc5874c2","ssoValidityInSecs":"5184000"}}
```

**This call may itself send an SMS or a push** (`currentGeneratedChallenge`).
Stop after it and inspect `details.currentChallenges` and
`details.currentGeneratedChallenge`. Only if nothing was generated, a second,
separately approved call to `generateChallenge` with
`CHANNEL_OPTIMIZED_PHONE_OTP` and `channel: "SMS"` (or `PUSH_V2`) sends
exactly one code. The user then passes the code to `grab auth login --otp`.

## 5. Why logins in the shared browser never stuck: the Track Order tab

The shared CDP browser had an old food.grab.com **Track Order** tab open
(`/ph/en/order/<order id>`). Once the browser's `gfc_session` lapsed, that
page kept polling the order's `/track` endpoint, got `401`, and handled it by
calling GrabID `POST /grabid/v1/oauth2/logout` and food `GET /auth/logout`,
about every 16 s. Any login done in the browser's normal profile, by hand or by
automation, was undone within seconds. That is why `gfc_session` never
stuck, and it plausibly explains the dead SSO record in §1, root cause 2 (a
logout revokes the server-side SSO session).

Consequences for grabcli:

- `grab auth login` uses an **isolated browser context**. It has its own
  cookie jar, so the stale tab's logout calls carry the default profile's
  cookies and can't reach the login's session.
- `grab auth login` lists the browser's tabs (`GET /json`, read-only) and
  **warns** about food.grab.com `/order/…` or `/track` tabs. It never closes
  other tabs. Close them by hand before logging in through the browser or
  running `grab auth import`.
- Open question: whether the stale tab's `oauth2/logout` can revoke SSO
  sessions other than the default profile's own. It sends that profile's SSO
  cookie, so it should only revoke that one, but this hasn't been verified.

### Page structure used by the driver (weblogin build of 2026-10-17)

Checked read-only in an isolated context. Nothing was typed or clicked:

- `food.grab.com/auth/login` → `weblogin.grab.com/passenger/login?…` with the
  OAuth params. In a background tab the SPA stays on its spinner until focus
  emulation (`Emulation.setFocusEmulationEnabled` +
  `Page.setWebLifecycleState active`) is on. After that, it renders.
- Phone form: country select (default `+63`), `input#Phone` (tel),
  "Remember me on this device for 60 days" (a checkbox, ticked by default),
  and a primary **Continue** button that stays disabled until a number is
  entered.
- Challenge picker (`challenge-options` route): `[role=button]` items, each
  with an `img alt="Proceed to <TYPE>"` (`PUSH_V2`, `PUSH`,
  `CHANNEL_OPTIMIZED_PHONE_OTP`, `SMS_OTP`, …). Taken from the bundle; not seen
  live.
- OTP screen (`optimised-sms-otp`): "Check your SMS|WhatsApp|…", a code input
  that submits itself when complete, and "Resend code" (which reveals the
  channel list). Errors include "Incorrect code. You can try N more times."
  Taken from the bundle.
- Push screen (`push-v2`): "Approve in mobile app" plus a QR code. It becomes
  "Request rejected" or "QR Code Expired" on failure. Taken from the bundle.

## Appendix: what was touched during this research

- `POST food.grab.com/auth/access-token` ×3, read-only for state. It returned
  401 and only re-set `next-i18next` and `gfc_country`.
- `GET food.grab.com/auth/login` ×3 and `GET partner-api…/oauth2/authorize` ×1,
  no redirects followed. These created anonymous PKCE contexts only.
- `GET …/oauth2/user_login_info` ×6 (read-only), and public
  `GET …/clients/{id}/weblogin-config` and `/public`.
- Static JS and HTML from weblogin.grab.com and food.grab.com.
- CDP: opened two background tabs (food `/auth/login` → weblogin
  saved-accounts, and weblogin `/help`), read DOM, localStorage key shapes and
  cookie names, then closed both. Nothing was clicked. Side effect: the
  browser's anonymous food.grab.com `gfc_session` was replaced by a new
  anonymous one. The browser was already logged out.
- Implementation checks (2026-10-17): two isolated contexts were created on
  the shared browser, loaded the weblogin phone form, were read (DOM, cookie
  names of that context only) and were disposed. Nothing was typed or
  clicked. One silent-refresh run used a *copy* of the session file. It
  stopped at authorize → weblogin without `auth_cookie_exists`, so the SSO is
  still dead. No code was exchanged.
