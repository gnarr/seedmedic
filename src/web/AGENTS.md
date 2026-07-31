# AGENTS.md — `src/web`

Supplements the root `AGENTS.md`. `src/web/settings/` is the only code in
SeedMedic that writes `config.toml`, and the only code that is ever allowed to
touch a secret's plaintext — read that constraint twice before changing
anything here.

## Shape

- **A React client in `web/`, over a JSON API under `api/`.** `spa.rs` serves the
  built bundle out of the binary; the shell and `/assets/*` are reachable without
  a credential and everything under `/api/v1` is not. That is a deliberate change
  from 0018's posture — guarding the shell gives `/` → `/login` → shell → 401 on
  its own asset, which is a blank page with no way in — and the bundle carries no
  operator data. `tests/web_auth.rs::the_shell_is_public_but_the_api_is_not` pins
  both halves. See `docs/todos/0021-a-react-operator-ui.md`.
- **Everything new goes under `src/web/`.** The plaintext-accessor grep at the
  bottom of `mod.rs` walks this directory, so an API module at `src/api/` would
  silently escape the one guard this file calls "the one thing here that must
  never regress silently". That test is a plain substring search, which is also
  why the accessor cannot be named literally anywhere under here — not even in a
  comment.
- **Nothing under `src/` may give `Secret` a `Serialize` impl, or
  `SafeRelativePath` a *derived* `Deserialize`.** Both are enforced by grep tests
  next to the types. `Config` derives only `Deserialize`, which is what makes "a
  secret reaches a browser" a compile error rather than a test — the primary
  guard, with the `.expose` grep as the second line. `SafeRelativePath`'s
  hand-written `Deserialize` is correct and must stay: it re-runs `parse`, and the
  hazard is a derive that would not.
- A request under `/api/` gets **401, never a redirect**. `fetch` follows a 3xx
  transparently, so a redirect hands the client HTML to parse as JSON. The
  `Accept`-based negotiation remains for the server-rendered pages.
- **Still `maud`: `/settings*`, `/status`, `/jobs/{id}` and `/login`.** The
  settings pages are not ported; `docs/todos/0021` says so and why.
- One runtime generation per request: every handler calls `state.runtime.current()`
  (or, for `/settings`, `ConfigDocument::read` plus the same `current()`) exactly
  once at the top, so a reload landing mid-request cannot mix generations. 0021
  adds exactly one documented exception, the event stream, which re-reads
  `current()` per emit because a stream that captured one generation would serve
  a staging root and tracker health from adapters replaced hours ago.
- The web module contains no rules of its own. If a decision here could come
  out differently than the worker's, it belongs in `config`, `repair`, or
  wherever the rule actually lives — see the root `AGENTS.md`'s "Architecture".
  `web/settings` in particular decides nothing about validity: it renders
  fields, turns a form into TOML, and asks `Config`'s own `Deserialize` and
  `problems()`/`problems_on_disk()`.

## Never call `Secret::expose` here

`src/web/mod.rs` has a blunt `#[test]` that fails if any file under `src/web/`
contains the text `.expose(`. It is deliberately not clever — no AST parsing,
just a substring search — because clever is exactly what would let a call
through by accident. If you find yourself wanting to expose a secret to render
it: you don't. `Secret::source()` (a `SecretSource`) is what the settings pages
read instead — `Environment`/`File`/`Inline`/`Unset`, never a value. See
`crate::config::SecretSource`.

## A form field's `name` is its dotted TOML key

`policy.max_attempts`, `trackers.0.base_url`, `arr.1.path_mappings.0.from`.
There is no separate form↔config mapping table, because the name *is* the
key `ConfigDocument::get`/`set`/`remove` take. This is also why
`web/settings/fields.rs`'s `FIELDS` table is the single source of truth for
what a settings page can show: `web/settings/save.rs::apply_pairs` looks every
submitted key up in it and rejects anything that doesn't match, which is
`deny_unknown_fields` at the HTTP layer.

## `Form<T>` cannot decode a field-level sequence

`serde_urlencoded`'s top-level deserializer supports "sequences of pairs" —
`Form<Vec<(String, String)>>` decodes a repeated key correctly, in order,
duplicates included. A **field** typed as a `Vec` does not: `serde_urlencoded`'s
per-value `Part` deserializer forwards `seq` to `deserialize_any`, which only
ever calls `visit_str`. Concretely, `#[derive(Deserialize)] struct Foo { id:
Vec<i64> }` used as `Form<Foo>` **422s over real HTTP** the moment two `id=`
pairs show up in the body — this already happened once, silently, because no
web test harness sent a real request to find out (see `tests/settings.rs` and
`tests/bulk_review.rs`). Every multi-value form in this module — bulk review
actions, `web/settings`'s whole submit pipeline — decodes
`Form<Vec<(String, String)>>` and works with the pair list directly.

## The hidden-input bool convention

A checkbox that is unchecked submits nothing at all, which is indistinguishable
from "this field doesn't exist" — wrong for a setting whose false state must be
written, not merely implied. The fix used everywhere in this module: a hidden
input immediately before the checkbox, same `name`, `value="false"`; the
checkbox itself is `value="true"`. Checked submits both (hidden, then
checkbox); unchecked submits only the hidden one. **The handler takes the last
value for a duplicated key** (`web/settings/save.rs::last_value_wins`), which is
exactly why a `Form<T>` with a real `bool` field would be wrong here — it takes
the *first* value, the opposite of what this convention needs.

## `*_file` paths are display-only

`server.auth_token_file`, `download_client.password_file`,
`trackers.*.api_key_file`, `arr.*.api_key_file` are rendered as read-only text,
never as an editable input, and `web/settings/save.rs::parse_field` refuses to
write one even if a crafted `POST` supplies a value for it
(`Kind::SecretFile => Ok(Parsed::Unchanged)`). These paths are deployment-level
by construction; making them editable turns an unauthenticated settings page
into a remote arbitrary-file-read primitive — point `api_key_file` at
`/etc/shadow`, save, and its contents leave as a bearer token on the next
request. Do not add an escape hatch for this "just for convenience."

## The write path

`ConfigDocument` (`src/config/write.rs`) is the only thing that turns a draft
back into bytes on disk, and it is deliberately not a `serde::Serialize` of
`Config`: `resolve_secrets` overwrites a secret's inline field in place at load
time, so by the time a `Config` exists in memory, an environment- or
file-sourced secret is indistinguishable from one typed inline. Serialising it
straight back out would write that value into `config.toml` in plaintext —
into a file most people keep in git — and it would then be silently *ignored*
on the next load, because env/file precedence still wins. `ConfigDocument`
edits the `toml_edit` document directly, one dotted key at a time, so an
untouched secret's line is untouched.

`web/settings/save.rs`'s pipeline order matters and mirrors
`docs/todos/0017-the-settings-pages.md` step 6: parse the form pairs, look
each key up in `FIELDS`, per-`Kind` lexical parse into a draft, `to_config()`
(serde's authoritative check, including `resolve_secrets` so a draft's
`problems()` reflects what would actually run), `problems()` +
`problems_on_disk()`, and only then `save()` + `RuntimeHandle::reload()`.
Validate before writing — the file on disk must never become a configuration
the process would refuse.

## Testing

`tests/settings.rs` is the only integration suite that drives a real
`bootstrap::open`/`RuntimeHandle::start` (not the `tests/support::Harness`
shortcut, which never touches `RuntimeHandle`) — because its acceptance test's
whole point is the real reload path: cold start, save from the browser,
reload, retry, watch a real background worker finish the job. Everything else
in `web/` unit-tests against a hand-built `Runtime`/`RuntimeHandle::fixed`
exactly as before; that harness cannot exercise a save or a reload and should
not be stretched to.
