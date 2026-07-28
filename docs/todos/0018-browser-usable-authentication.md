# 0018 — Browser-usable authentication

**Status:** Not started
**Depends on:** 0016
**Blocks:** nothing — but it must land before 0017 ships

## Problem

`require_auth_token` accepts the token only as `Authorization: Bearer <token>`.
A browser will not send that header from an HTML form, so **the entire operator
UI is unusable whenever `server.auth_token` is set**, unless a reverse proxy
injects the header. The README half-acknowledges this by describing the token as
"enough to keep it off casual scans behind a reverse proxy".

Today that is merely awkward. After 0017 it is a trap: the settings page will
offer an `Authorization`-shaped field called "auth token", and saving it would
immediately lock the operator out of the page they saved it from, with no way back
except editing the file the UI exists to avoid editing.

There is a second problem that only appears once a cookie exists. No POST in the
UI carries a CSRF token. That has been safe precisely *because* a browser never
sent the credential: with a token set, a cross-site POST is rejected for lack of
the header, and with no token set there is nothing to forge. A cookie makes every
POST reachable from any page the operator happens to visit — and after 0017 the
ceiling on that is "set `library.roots = []`, set `staging.root` inside the media
tree, save".

## Architectural context

`0011` resolved that the auth story is "an optional shared secret, not a login
system", and that user accounts, roles and OIDC are out of scope. That stands.
This document does not add a login system; it moves the *existing* shared secret
somewhere a browser can carry it, and closes the CSRF hole that move opens.

`0010` also recorded "the UI is unauthenticated and assumed to be on a trusted
network" as out of scope. That assumption is exactly what 0017 makes expensive,
which is why this document exists now.

## Expected behaviour

- With `server.auth_token` set, a browser can use the whole UI after entering the
  token once.
- `Authorization: Bearer` keeps working unchanged, for scripts and for anything
  already automated against it.
- A browser request without credentials gets a login page; a script request with
  a bad bearer token gets a plain 401, not a redirect.
- Signing out works, and does not require restarting the process or rotating the
  token.
- Cross-site POSTs are rejected.
- With no token set, behaviour is unchanged: everything is open, and 0015's loud
  warning says so.

## Implementation steps

1. **`GET /login`** renders one password field and nothing else. It is exempt from
   the auth middleware, as `/health` already is.

2. **`POST /login`** compares the submitted value against `server.auth_token` and,
   on success, sets a cookie:

   ```
   Set-Cookie: seedmedic_session=<id>; HttpOnly; SameSite=Strict; Path=/
   ```

   plus `Secure` when the request arrived over HTTPS (direct TLS or
   `X-Forwarded-Proto: https`). On failure it re-renders with a message and sets
   no cookie.

3. **The cookie carries a random session id, not the token.** A 32-byte value
   from the OS, held in an in-memory `Mutex<HashSet<_>>` on the `RuntimeHandle`
   side of the process, cleared on restart and when the token changes.

   The cheaper option — put the token itself in the cookie, no state at all — was
   rejected. A leaked cookie would then be full API access forever, with no way to
   revoke it short of rotating the token everywhere; and there would be no real
   logout, only a cleared cookie the holder of the value can ignore. The session
   set is about thirty lines, needs no dependency and no database, and gives both.

4. **The middleware accepts either credential**: the bearer header, or a session
   cookie whose id is in the set. It reads the expected token from
   `state.runtime.current().auth_token` (0016), so a token set in the UI takes
   effect on the very next request rather than at the next restart.

5. **Setting a token through the UI must not lock the operator out.** When 0017
   saves a new `server.auth_token`, mint a session for the operator who saved it
   in the same response. They stay signed in; the next visitor does not.

6. **CSRF.** `SameSite=Strict` is the primary control and is load-bearing, not
   decoration — say so where it is set, so nobody "simplifies" it to `Lax` later.
   Add, because `SameSite` is a same-browser control and not a same-origin one:

   - reject any POST whose `Sec-Fetch-Site` is present and not `same-origin`
     (one header check, and it covers the existing job-action forms too);
   - fall back to an `Origin` check when `Sec-Fetch-Site` is absent.

   A per-form hidden token is deliberately **not** added: with a session set
   already in hand it would be the third overlapping control, and the two above
   cost four lines between them.

7. **Constant-time comparison.** Add `Secret::verify(&self, candidate: &str) ->
   bool` folding over bytes, and use it. This replaces the current plain `==`.
   Timing-attacking a `str` compare through TCP, tokio and axum is not a realistic
   attack and this is not the important finding in this document — but a login page
   makes repeated automated attempts free, it is five lines and no crate, and it
   means the web layer never needs `expose()` at all, which is a property 0017
   tests for.

8. **Content negotiation for the rejection.** An HTML request (`Accept` contains
   `text/html`) gets `303` to `/login`; anything else gets `401` with the current
   plain-text body. A script that starts silently following redirects to a login
   page instead of failing is worse than a script that gets a 401.

9. **`POST /logout`** removes the session and clears the cookie.

10. **A "Sign in" / "Sign out" affordance** in the page chrome added by 0015, and a
    banner recommending a token when none is set (also 0015 — this document only
    makes the recommendation actionable).

11. **Docs.** `README.md`'s security paragraph currently describes bearer-only
    auth and is made false by this. It should say: a token, entered once in the
    browser or sent as a bearer header; still no accounts and no roles; still not a
    substitute for TLS or network-level access control.

## Invariants and safety constraints

- **The token is never rendered, logged, or put in a URL.** In particular it never
  appears in a `Location` header — a test asserts that, because a redirect that
  carries the credential would leak it into proxy logs and browser history.
- **The cookie is `HttpOnly` and `SameSite=Strict`**, and `Secure` whenever the
  request arrived over HTTPS. `SameSite=Strict` alone does nothing against a
  plain-HTTP intermediary on a LAN, which is one more reason the README must keep
  saying "do not expose this".
- **`/health` stays exempt**, so an orchestrator does not need the token.
  `/login` is exempt for the obvious reason. Nothing else is exempt — in
  particular the settings routes are not.
- **No accounts, no roles, no password hashing, no persistence of sessions.** The
  token is the whole secret; sessions are in-memory and disposable.
- Sessions are invalidated when the token changes.

## Likely files

- `src/web/mod.rs` (middleware, routes), `src/web/login.rs` (new)
- `src/web/layout.rs` (sign in/out affordance)
- `src/config/mod.rs` (`Secret::verify`)
- `src/runtime.rs` (the session set)
- `README.md`

## Required tests

Extending `tests/web_auth.rs`:

- With a token set and no credential: an HTML request gets 303 to `/login`; a
  non-HTML request gets 401.
- `GET /login` is 200 without credentials.
- The right token sets a cookie carrying `HttpOnly` and `SameSite=Strict`, and
  redirects.
- That cookie authorises a subsequent request; a made-up session id does not.
- The wrong token re-renders with no `Set-Cookie`.
- `Authorization: Bearer` still works; a bad bearer is 401, never a redirect.
- `POST /logout` clears the session, and the cookie no longer authorises.
- `/health` is reachable with no credential (the existing test).
- The token appears in no `Location` header and in no response body.
- A POST with `Sec-Fetch-Site: cross-site` is rejected; `same-origin` is allowed.
- Changing the token invalidates existing sessions.
- With no token set, every existing test still passes unchanged.

## Acceptance criteria

- An operator can set `server.auth_token`, stay signed in, and keep using the UI.
- A fresh browser is asked for the token once and then works normally.
- `curl -H 'Authorization: Bearer …'` is unaffected.
- A cross-site form POST cannot trigger a job action or a settings save.

## Out of scope

- User accounts, roles, OIDC, password hashing, multiple tokens, token rotation
  from the UI.
- Rate limiting or lockout on failed logins. Worth revisiting if anybody exposes
  this deliberately; the documented posture is still "do not".
- TLS termination. Still a reverse proxy's job.

## Open questions

- Cookie carries the token, or a session id?

  **Resolved:** a session id. See step 3 — a leaked cookie should not be
  permanent full access, and logout should mean something. The in-memory set is
  cheap enough that the stateless version is not worth its downsides.

- Is HTTP Basic auth enough, given it needs no new pages?

  **Resolved:** no. It is about fifteen lines against roughly eighty, but the
  browser's credential dialog presents itself as a login system this is not, there
  is no logout short of closing the browser, and it cannot show the "no token set"
  guidance or a useful error. The page is worth the difference.

- Is a per-form CSRF token needed on top of `SameSite=Strict`?

  **Resolved:** no. `SameSite=Strict` plus the `Sec-Fetch-Site` check covers the
  threat for this deployment shape, and both apply to the job-action forms that
  already exist without touching them. A hidden token would be a third
  overlapping control and would have to be threaded through every form in 0017.

- Should the settings pages be reachable when no token is set?

  **Resolved:** yes — decided when this work was scoped, and it matches the
  posture already documented in the README as well as how comparable self-hosted
  applications onboard. It is made materially safer by 0017's rule that `*_file`
  paths are display-only, which removes the arbitrary-file-read primitive, and by
  0019's rule that a connection test uses only the secrets present in the form,
  which removes the stored-credential exfiltration path. What remains is that
  anyone who can reach the port can reconfigure the instance — which is what the
  loud warning from 0015 is for.
