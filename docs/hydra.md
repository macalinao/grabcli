# The hydra device JWT

GrabFood's menu, quote and order endpoints require an `x-hydra-jwt` request
header. Search does not. Without a valid one, those endpoints return
`400 {"reason":"invalid_argument","message":"invalid hydra jwt"}`.

## What it is

An RS256 JWT with a ~10 minute TTL. Claims:

```json
{
  "iss": "gaa", "aud": "gfw", "name": "grabtaxi",
  "iat": <now>, "exp": <now+600>, "nbf": <now>,
  "ver": "1.19.0.24",
  "brID": "<browser fingerprint>", "brIDv2": "<…>", "brUID": "<uuid>",
  "sus": false
}
```

`brID`/`brIDv2`/`brUID` are browser-fingerprint strings and `sus` ("suspicious")
is a boolean. The web app computes these with a device-scan SDK; the server
validates the token's **signature**, not the fingerprint contents.

## How the web app makes it

The SDK (`ScanDeviceID`, module `uJ3e` → loader `PLSw` in the app's
`common-utils` bundle) lazy-loads a chunk that:

1. runs a fingerprintjs-style scan to collect the `brID*` fields, then
2. signs the claims **client-side** with an RSA private key embedded in that
   same public JavaScript bundle, and caches the token in
   `sessionStorage.hydra_device_token`.

A separate fire-and-forget telemetry POST (to a Grab "guardian" analytics
endpoint) also happens, but its response is **not** used to build the token. So
no server round-trip is needed to mint one.

The decisive finding: because the signing key ships in the public web bundle,
anything that has that key can mint a valid token offline. There is no
proof-of-work, no WASM VM gate, and no server-issued secret.

## How grabcli mints it

Two modes, chosen by `hydra_mode` in config (`auto` by default):

### Native (`hydra_mode = "native"`, or `auto` with a key present)

grabcli builds the claims, generates plausible fingerprint values, and signs
with an RS256 key read from a PEM file (`~/.config/grabcli/hydra-key.pem`, or
the `hydra_key` config path). This needs no browser and is instant, so it's the
default when a key is available.

**The signing key is not shipped with grabcli.** You supply it yourself. It is
the key the web app uses, which you can recover by reading the web app's
`ScanDeviceID` chunk (`getJWTKey()` returns it base64-encoded). Put the PEM at
`~/.config/grabcli/hydra-key.pem` with mode 0600.

Caveats: if Grab rotates the embedded key or the `ver` string, native minting
will start returning `invalid hydra jwt`; re-extract the key / bump the version,
or switch to CDP mode. Server-side anomaly checks on the fingerprint fields are
possible in principle but weren't observed.

### CDP (`hydra_mode = "cdp"`, or `auto` with no key)

grabcli drives a logged-in browser over the Chrome DevTools Protocol: it opens a
`food.grab.com` page, lets the page mint a token the normal way, and sniffs the
`x-hydra-jwt` header (and the app version) off the network events. Slower (a few
seconds) and needs a running logged-in browser, but requires no signing key.

## Refresh

Before any hydra-requiring call, grabcli checks the stored token's `exp` (with a
small skew) and mints a fresh one if needed. If Grab rejects a token mid-call
(the `invalid hydra jwt` 400), grabcli re-mints once and retries, as long as
`auto_refresh` is on (the default).

## Verdict

Native minting is feasible and is the default, because the signing key lives in
the public web bundle. CDP remains as a key-free fallback and for environments
where you'd rather let a real browser produce the token.
