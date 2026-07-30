# 0019 — Connection tests

**Status:** Done
**Depends on:** 0017
**Blocks:** nothing

## Problem

Nothing verifies that a configured integration is reachable, or that its
credentials are valid, until a repair needs it. Startup checks only that an API
key is *present*. A wrong qBittorrent password, a revoked tracker token, a URL
with a typo, or an *arr on a port that moved are all discovered on the first
worker tick that needs them — which is up to `discovery_interval` later, in a log
line, with a repair parked for review.

For somebody configuring SeedMedic for the first time, that is the difference
between "your API key is wrong" and "nothing seems to be happening". It is the
single highest-value addition to the settings UI, and it is cheap, because every
method it needs already exists.

`0011` deferred this as a possible `--check-connections`, on the grounds that
nothing needed it and building it speculatively would be unowned complexity. That
reasoning no longer holds: 0017 gives it a real caller and a real user.

## Architectural context

**No new ports and no new port methods.** `AGENTS.md` forbids adding methods to a
port for capabilities no use case needs, and all three probes are satisfied by
methods that exist and are already exercised:

| Target | Method | Why it is sufficient |
|---|---|---|
| Download client | `TorrentClient::summary()` | Already documented as "proves the client is reachable at all, not just that one known torrent is", and already used by `/status`. It also proves credentials, because `QBittorrentClient` logs in first, and `ClientError` distinguishes `Unauthorized` from `Transport`. |
| Tracker | `TrackerClient::list_hit_and_runs()` | Proves base URL, token placement and credentials in one call, and the count it returns is genuinely informative — "reachable, 2 outstanding" is a better answer than "reachable". |
| *arr | `CandidateSource::find_candidates()` with a probe title | Verified: an unrecognised release returns `Ok(vec![])` after issuing `api/v3/parse` — there is no early return for an empty file list — and 401/403 maps to `Protocol("… rejected the configured API key")`. So `Ok(_)` proves reachability *and* that the key was accepted. |

A tracker `ping()` would be faster but it is exactly the speculative port method
the rules forbid, and it would prove *less*: an instance that answers `/api/` but
rejects the token would pass it.

**The probes live beside `bootstrap`, not in `web`.** `bootstrap.rs` is the only
module allowed to name a concrete adapter, and a probe has to build a throwaway
adapter from the values currently in the form. A new `src/connectivity.rs`
alongside it is the natural home, and it gives `--check-connections` the same
entry points as the UI.

## Expected behaviour

- A "Test" button beside each tracker, each *arr instance, and the download
  client, which tests **the values currently in the form** — so credentials can be
  proven before they are committed.
- A test never saves anything and never changes the running configuration.
- The result names what failed in the adapter's own words: rejected credentials,
  unreachable host, unparseable response.
- A test cannot hang.
- `seedmedic --check-connections` reports the same thing for the saved
  configuration, for operators who never open a browser.
- `--check-config` still touches no network.

## Implementation steps

1. **`src/connectivity.rs`:**

   ```rust
   pub struct ProbeResult { pub ok: bool, pub detail: String }

   pub async fn test_tracker(config: &TrackerConfig) -> ProbeResult;
   pub async fn test_download_client(config: &DownloadClientConfig) -> ProbeResult;
   pub async fn test_arr(config: &ArrConfig) -> ProbeResult;
   ```

   Each builds exactly one adapter and calls exactly one method.
   `bootstrap::build_client` changes from taking `&Config` to
   `&DownloadClientConfig` so a single instance can be built without a whole
   config.

2. **A dedicated probe HTTP client** with `.timeout(Duration::from_secs(10))`.
   `Unit3dTracker::list_hit_and_runs` self-throttles to one request per 500 ms and
   will follow pagination up to `MAX_PAGES = 1000`, so an unbounded probe against
   a large account is a hang, not a slow answer. Wrap the whole call in
   `tokio::time::timeout` as well, so a slow adapter cannot outlast the request.

   Worth noting while here: `bootstrap::build_http_client()` sets **no timeout at
   all** today, for every adapter. That is a separate latent problem and should be
   fixed separately rather than folded in.

3. **Probe titles for *arr.** Use something that genuinely parses as a release —
   `"Test.Show.S01E01.1080p.WEB-DL.x264-GROUP"` — so `api/v3/parse` does real work
   and simply matches no series, rather than being rejected as malformed.

4. **The UI.** A second submit button per section using `formaction`, so the whole
   draft posts:

   ```html
   <button formaction="/settings/trackers/0/test">Test connection</button>
   ```

   The handler shares one `draft_from(submitted)` function with the save path —
   that shared function is the reason the two behave identically — then builds the
   one adapter, probes, and re-renders the same form with a result panel and every
   submitted value preserved. It never writes.

5. **`--check-connections`**, about fifteen lines in `main.rs`: load the config,
   probe every configured integration, print one line each, exit non-zero if any
   failed. Two callers for the same function, no new abstraction.

6. **Amend `0011`'s resolved open question** with a pointer here, rather than
   rewriting it — the deferral was correct at the time and the reason it changed is
   worth keeping.

7. **`README.md`** gains a sentence in the security paragraph: the settings UI can
   make outbound requests to addresses you type into it, which is another reason
   not to expose the port.

## Invariants and safety constraints

- **A probe uses only the secrets present in the form.** For a test, an empty
  secret field means *unset*, not *unchanged*: the handler refuses with "Enter the
  API key to test this connection" rather than reaching for the stored value.

  This is not a nicety, it is the mitigation for the one real exfiltration path.
  Without it, anyone who can reach an unauthenticated settings page can point
  `download_client.base_url` at a host they control, leave the password box empty,
  press Test, and receive the operator's stored qBittorrent password. One branch
  closes it.

- **A probe never writes anything** — not `config.toml`, not the database, not the
  staging area. A test asserts the config file's contents and mtime are unchanged.

- **Never render the response body.** Report the mapped error variant and a
  message truncated to about 200 characters. `maud` escapes by default, but
  `ClientError::Rejected(String)` can carry a fragment of a remote response, so
  truncate as well as escape.

- **Secrets stay out of errors.** Every adapter already calls
  `error.without_url()` before converting a `reqwest::Error`, which is what keeps
  a token in a query string out of the message. Do not bypass it.

- **No SSRF allow-list or deny-list.** Recorded as a deliberate non-goal so nobody
  later "hardens" it into uselessness: blocking RFC1918 would break
  `http://qbittorrent:8080`, which is the only deployment shape that actually
  exists. The honest description of this feature is that it is an authenticated
  arbitrary-outbound-GET primitive, and the mitigations are the auth token (0018),
  the `*_file` display-only rule (0017), and the empty-secret rule above.

- `--check-config` remains network-free. That property is what makes it safe to
  run against a production config from anywhere, and it is not weakened by adding
  a separate flag.

## Likely files

- `src/connectivity.rs` (new)
- `src/bootstrap.rs` (`build_client` signature)
- `src/web/settings/` (the test handlers)
- `src/main.rs` (`--check-connections`)
- `README.md`, `docs/todos/0011-configuration-and-secrets.md`

## Required tests

- **A probe with an empty secret is refused before any request is made** — assert
  the wiremock request count is zero. This is the security test; it must not be a
  test that merely checks the message.
- With `wiremock`, following the existing patterns in the *arr and qBittorrent
  adapters:
  - tracker probe against a healthy mock reports success and the count;
  - 401 reports rejected credentials;
  - a refused connection reports a transport failure;
  - a server that never responds reports the timeout rather than hanging;
  - *arr probe with a valid key and an unmatched release reports success;
  - *arr probe with 403 reports the rejected key;
  - download-client probe reports the torrent count, and a wrong password reports
    rejected credentials.
- A probe uses the submitted draft values, not the live configuration — assert the
  mock received the draft's key, not the saved one.
- A probe leaves `config.toml` byte-identical, with an unchanged mtime.
- An error message is truncated and HTML-escaped.
- `--check-connections` exits non-zero and names the failing integration.

## Acceptance criteria

- An operator can prove their tracker token, qBittorrent password and *arr key
  from the settings page before saving them.
- A wrong credential says so, in words that name which service and what was
  wrong.
- No probe can be made to reveal a stored secret.
- `seedmedic --check-connections` answers the same question headlessly, and
  `--check-config` still makes no network request.

## Out of scope

- Probing on a schedule, or a health page that polls integrations. `/status`
  already shows passively collected tracker health and download-client
  reachability.
- Probing the notification webhook. It is fire-and-forget by design and a test
  POST would deliver a real notification.
- Fixing `build_http_client`'s missing timeout for the production adapters. Real,
  but separate.
- Any change to what `/health` reports. It stays deliberately blind to trackers
  and the download client, because those being down is normal and recoverable.

## Open questions

- Should a tracker probe use a cheaper call than `list_hit_and_runs`?

  **Resolved:** no. It is the heaviest call the adapter makes, but it is
  operator-initiated, bounded by the timeout, and it is the only call that proves
  the token is accepted rather than merely that something is listening. If the
  page count ever becomes a real problem, the fix is a bounded variant of the
  existing method, not a new port method.

- Should `CandidateSource` gain a `probe()` method?

  **Resolved:** no. The synthetic-query probe was verified to reach
  `api/v3/parse` and to surface a rejected key, so a method would add port
  surface for nothing. If an *arr ever turns out to return a hard error for an
  unmatched parse, change the probe title first; only add a method if
  `--check-connections` and the UI both need it, which would be two real callers
  rather than one.

- Should `--check-connections` be folded into `--check-config` behind a flag?

  **Resolved:** no. Separate flags keep the guarantee that `--check-config`
  touches no network, which is what makes it safe to run anywhere. Argument
  parsing stays the hand-rolled `args().nth(1)` comparison; two flags do not
  justify `clap`.
