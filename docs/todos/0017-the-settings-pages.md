# 0017 — The settings pages

**Status:** Done
**Depends on:** 0014, 0016
**Blocks:** 0019

## Problem

Every setting still has to be typed into a TOML file by hand. 0015 makes an
unconfigured SeedMedic start and say what is missing; this document is what lets
somebody act on that without leaving the browser.

`docs/todos/0011` listed "Configuring through the web UI" under **Out of scope**.
That is superseded here, and amended in place rather than contradicted silently.

## Architectural context

The operator UI is server-rendered `maud`, no JavaScript, one inline stylesheet,
no asset pipeline, and no API surface beyond what the pages need. Those
constraints hold. They are what make this a few hundred lines rather than a
frontend.

The web module contains no rules of its own, by design — "a decision the UI could
make differently from the worker is a decision in the wrong place". This document
honours that in a specific way: **the UI never decides whether a configuration is
valid.** It renders fields, turns a form into TOML, and asks `Config`'s own serde
derive and `problems()` (0014). So the UI cannot save something startup would
reject, and it cannot accept something startup would allow.

Three facts about the existing stack shape the whole design, and all three were
verified rather than assumed:

- **`Form<Vec<(String, String)>>` works today, with no new dependency.**
  `serde_urlencoded`'s top-level deserializer documents support for "sequences of
  pairs", and its `deserialize_seq` calls `visit_seq` on the inner
  `MapDeserializer`, which yields `(K, V)` tuples. So a form body decodes to an
  ordered pair list, repeated names included.
- **A *field-level* `Vec` cannot deserialize at all.** The per-value `Part`
  deserializer forwards `seq` to `deserialize_any`, which calls `visit_str`. This
  is not theoretical: `review::BulkForm { id: Vec<i64> }` means
  `POST /jobs/bulk/retry` and `/jobs/bulk/abandon` **return 422 over real HTTP
  today**. Nothing noticed because `tests/bulk_review.rs` says outright that no
  web test harness exists and calls the store directly. This document fixes it,
  and the fix earns a test that fails against `main`.
- **`Secret::expose()` is called in exactly five places, none under `src/web/`.**
  Verified: zero hits. That is a property worth a test, because this document is
  where it would be broken.

## Expected behaviour

- Every setting in `Config` is viewable and editable from `/settings`.
- A bad value produces a message next to that field, with the operator's other
  typing preserved, and nothing is written.
- Saving writes `config.toml`, preserving comments, key order, and every key the
  operator did not change, then reloads (0016).
- A secret's value is never rendered, logged, or echoed back. A secret coming
  from the environment or a `*_file` is shown read-only with its source named.
- A secret is never written into `config.toml` unless the operator typed it.
- When the config file cannot be written, the UI says so before the operator
  types anything, and offers the equivalent TOML — redacted — to copy.
- Dangerous settings carry their warning next to the control.
- The two restart-required keys say so.

## Implementation steps

1. **`toml_edit`.** One new crate; its transitive dependencies (`indexmap`,
   `winnow`, `toml_datetime`, `toml_parser`, `toml_writer`) are already in
   `Cargo.lock` via `toml` 1.1. Run `cargo tree -d` and confirm no duplicate
   versions appear — `toml` 1.1 and `toml_edit` are separate stacks and this is
   the one real risk of adding it. The alternative is a hand-rolled
   comment-preserving TOML rewriter, which is strictly worse. Serialising `Config`
   with serde is not an alternative at all: see the secrets invariant below.

2. **`ConfigDocument`**, in `src/config/write.rs` (split the 1137-line
   `config.rs` into `config/mod.rs` plus this):

   ```rust
   /// The config file as text, with comments and key order preserved.
   /// Deliberately no `Debug`: the document contains inline secrets.
   pub struct ConfigDocument {
       path: PathBuf,
       doc: toml_edit::DocumentMut,
       /// `false` when the file or its directory is not writable.
       writable: bool,
       /// The original file's unix mode, so a save does not widen `0o600`.
       mode: Option<u32>,
       /// Length and mtime at read time, to detect an external edit.
       stamp: Option<(u64, SystemTime)>,
   }
   ```

   with `read`, `get`, `set`, `remove`, `to_config`, `save`, and
   `to_redacted_toml`. `get` returning `None` means the key is absent, so the form
   shows the default as a placeholder rather than pretending it was set.

3. **The write path**, in order: refuse if the current file does not parse;
   refuse if the length/mtime stamp changed since the page was rendered; write
   `config.toml.bak`; write a temp file in the same directory with mode `0o600`
   at creation; `fsync`; `rename` over the target; **fall back to an in-place
   truncate-and-write when the rename fails**, and report which happened.

   The fallback is not defensive padding. `docker-compose.yml` currently mounts
   `./config.toml:/config/config.toml:ro` — a read-only *single-file* bind mount.
   Even without `:ro`, you cannot `rename()` over a bind-mount point; Linux
   returns `EBUSY`. A Kubernetes ConfigMap mount is worse: it is a symlink farm,
   so a rename replaces the symlink and the next kubelet sync silently reverts the
   save. In-place write is the only thing that works over a bind-mounted file.

4. **Writability, detected up front.** Probe the file and its directory at read
   time. `Dockerfile` declares `VOLUME ["/config", ...]`, which creates a
   root-owned `0755` anonymous volume, and the image runs as uid 10001 — so a
   default `docker run` cannot even create a temp file in `/config`. The UI must
   say that on the page, naming the path, **before** the operator fills in a form,
   not as a 500 afterwards.

   **Amended by `docs/todos/0020-a-container-that-just-runs.md`.** The specific
   case above no longer exists: the image declares no `VOLUME` at all, and an
   entrypoint takes ownership of the mounts before dropping privileges. The
   requirement is unchanged and the probe stays — a read-only mount, a
   Kubernetes ConfigMap and an NFS export can each still make the file
   unwritable, and 0020 deliberately keeps the container's root filesystem
   unwritable so that a mistyped `staging.root` is refused inline by this very
   check. Only the example that motivated it is now historical.

5. **The field table**, `FIELDS`, in `src/web/settings/fields.rs`:

   ```rust
   pub enum Kind {
       Bool,
       Count { unit: Option<&'static str>, min: u64 },
       Text,
       Url,
       AbsolutePath,
       AbsolutePathList,                 // textarea, one per line
       Choice(&'static [&'static str]),  // serde names, so exactly what the file accepts
       Secret { env_var: SecretEnv },    // never rendered with a value
       SecretFile,                       // display-only; see the invariants
   }

   pub struct Field {
       /// The dotted TOML key, which is also the form field's `name`. Repeated
       /// sections use `*`: `trackers.*.base_url`.
       pub key: &'static str,
       pub label: &'static str,
       pub help: &'static str,
       pub kind: Kind,
       /// Rendered with a warning and a confirmation. `policy.allow_hardlink` is
       /// the entire reason this exists.
       pub danger: Option<&'static str>,
       pub restart_required: bool,       // true for exactly two keys
   }
   ```

   The `help` prose already exists as comments in `config.example.toml` and doc
   comments in `config.rs`. This is relocation, not new writing.

6. **Submit → typed config. The form field's `name` *is* the TOML key** —
   `policy.max_attempts`, `trackers.0.base_url`,
   `arr.1.path_mappings.0.from`. Because of that there is no form↔config mapping
   table to drift. One handler pipeline, shared by every section:

   1. `Form<Vec<(String, String)>>` — the pair list, in document order.
   2. Look each key up in `FIELDS`. **Unknown keys are rejected** — this is
      `deny_unknown_fields` at the HTTP layer, and it stops a crafted POST from
      inventing `[[trackers]]` entries the page never rendered.
   3. Per-`Kind` lexical parse into a clone of the current document. `"abc"` for a
      `Count` fails here, attributed to its key, with no TOML involved.
   4. `draft.to_config()` — serde does the authoritative check, including enum
      variants, `Url`, and `deny_unknown_fields`.
   5. `problems()` + `problems_on_disk()`. `Error`s block and attach to their
      `key`; `Warning`s are shown and do not block.
   6. Only then `save()`, then `RuntimeHandle::reload()`.

   Validate before writing, so the file on disk is never a configuration the
   process would refuse. The alternative — write, reload, restore from `.bak` on
   failure — has more states and a restore that can itself fail.

   Step 3 catches everything step 4 could, so step 4's error is a page-level
   last-resort fallback. A test asserts that for every field a deliberately bad
   value produces a *field-level* message and never the fallback, which is what
   keeps the fallback from becoming load-bearing. Mapping
   `toml::de::Error::span()` back to a key is therefore not needed and is
   deliberately not built.

7. **The drift test, in both directions.** This is the load-bearing piece: it is
   what makes "every setting is editable" true next year instead of only today.

   - *Every `FIELDS` key exists in `Config`*: for each field, build minimal-valid
     TOML plus that one key with a type-appropriate value and
     `toml::from_str::<Config>` it. `deny_unknown_fields` fails a renamed or
     removed key. ~20 lines.
   - *Every `Config` key is in `FIELDS`*: deserialize a document with one
     deliberately bogus key per table and parse the ``expected one of `a`, `b` ``
     list out of serde's error message. That yields a true, serde-derived
     enumeration of every field name per table, with no macro and no reflection.
     ~40 lines. Guard it by asserting at least one backticked name was found
     before comparing, so a serde wording change fails loudly instead of silently
     passing with an empty set.

8. **The awkward cases, each with a decided rule.**

   - **Bool "absent means false".** Render a hidden input immediately before the
     checkbox, same name: `value="false"` then the checkbox `value="true"`.
     Checked submits both, unchecked submits only the hidden one, and **the
     handler takes the last value for a duplicated key**. Test the convention.
     This is also exactly why a `Form<T>` with a `bool` field would be wrong — it
     takes the first.
   - **Empty string means `None`**, not `Some("")`, for every optional field.
     There are seven: `server.auth_token_file`, `download_client.password_file`,
     `download_client.category`, `trackers[].api_key_file`,
     `arr[].api_key_file`, `notifications.webhook_url`, and `staging.root`. Getting
     this wrong is not cosmetic: `Some(PathBuf::from(""))` for a `*_file` makes
     `resolve_secret` try to read `""` and the config refuses to load.
   - **`library.roots`** is a textarea, one absolute path per line; blank lines
     ignored; errors are per-line against the one field ("line 2: `media/tv` must
     be an absolute path"). A path containing a newline is unrepresentable — say
     so in the help. This treatment applies to nothing else;
     `arr[].path_mappings` has two fields per row and gets the repeated-block
     treatment.
   - **Repeated sections.** `FIELDS` carries templates with `*`; rendering
     iterates the document's array length plus one blank "add" block; names carry
     the concrete index. **Removal is an explicit `POST
     /settings/trackers/{i}/remove` behind a confirmation**, never "clear all the
     fields" — a half-cleared row is ambiguous. The confirmation page must state,
     in those words, how many unfinished jobs a removal orphans, because 0016
     refuses the change if there are any.
   - **Minimal writes.** Write only keys whose submitted value differs from the
     currently effective one. Keeps the file small and `git diff` meaningful.

9. **Pages, not one page.** `/settings` index plus one page per section. A
   45-field page is unreviewable, a save touching one section gives a meaningful
   "what changed", and a validation error re-renders something small. `metrics`
   and `notifications` share an "Integrations" page (three fields). `policy`'s
   fifteen stay together — it is cohesive, and it is the page an operator will
   actually read.

10. **Secrets.** Add provenance to `Secret`, because without it this whole section
    has no data to work from:

    ```rust
    /// What the settings UI is allowed to know. Deliberately has no variant
    /// carrying a value, so there is nothing to render by accident.
    pub enum SecretSource {
        Unset,
        Environment { var: String },  // wins over everything, so read-only
        File { path: PathBuf },       // also wins over inline, so read-only
        Inline,                       // the only source the UI can change
    }
    ```

    `resolve_secret` already computes this; it just has to return it.
    `#[serde(transparent)]` cannot coexist with a second field, so hand-write
    `Deserialize` (six lines). `Secret`'s derived `Eq`/`PartialEq` are unused
    outside `config.rs` — drop them, and the "does equality compare sources"
    question disappears.

    Per secret, the affordance is:

    - A status line, always: `Set — from SEEDMEDIC_TRACKER_DEMO_API_KEY` /
      `Set — from /run/secrets/demo` / `Set — in config.toml` / `Not set`.
    - Source `Environment` or `File`: **no input at all**, plus "Managed outside
      SeedMedic. Unset the variable / clear `api_key_file` to edit it here."
    - Source `Inline` or `Unset`: `<input type="password" value=""
      autocomplete="new-password">` — `value` unconditionally empty — with a
      `••••••••` placeholder when set, and a separate `…api_key.clear` checkbox.

    **An empty secret input means leave the stored value alone.** Clearing
    requires the checkbox. This is the only rule that is safe with an always-empty
    `value`: "empty means clear" would silently wipe every secret in a section
    whenever the operator saved an unrelated field on the same page.

11. **Dangerous settings** carry the README's own wording next to the control:
    `policy.allow_hardlink` ("a hardlinked staged file *is* the library file") and
    `policy.auto_resume`. Both go behind a confirmation step.

12. **Restart-required.** `server.bind_address` and `database.path` are written
    and flagged ("restart to bind …"), never applied. Nothing attempts to rebind a
    live listener.

13. **`POST /settings/load-demo`** writes today's `config.example.toml`
    fake-tracker setup, with `staging.root` derived as an absolute path from the
    config file's own directory, then reloads — so the zero-config demo survives.
    Only offered when the current config has no trackers, only when built with
    `fakes`, and the route returns 409 under `--no-default-features`. Honours the
    same `.bak` rules as any other save.

14. **`metrics.enabled` needs the build capability shown next to it.** The
    `metrics` cargo feature is not in the default set and `/metrics` is
    `#[cfg]`-gated, so on a default build the checkbox enables nothing and today's
    only signal is a log warning. Either render the checkbox disabled with "this
    build does not include the `metrics` feature" or hide it.

15. **Fix `BulkForm`** with the same pair-list decoding, and add the over-HTTP
    test that is currently missing.

16. **`src/web/AGENTS.md`**, new. The web module is about to roughly double and to
    gain both the only code that writes a config file and the only code that must
    never call `expose()`. State: no JavaScript, no asset pipeline; one runtime
    generation per request; never call `Secret::expose`; a form field's `name` is
    its TOML key; the hidden-input bool convention; and that `Form<T>` cannot
    deserialize a field-level sequence, so bodies are decoded as `Vec<(String,
    String)>`. That last line alone justifies the file — it is the trap that
    already bit `BulkForm`.

17. **Docs.** `README.md`: "Try it" becomes run-and-open-the-UI; "Configuration"
    gains `/settings` while keeping the file as the source of truth.
    **`docker-compose.yml` must change from `./config.toml:/config/config.toml:ro`
    to a writable directory mount `./config:/config`** — call this out as a
    breaking change for anyone following the current README, because a single-file
    mount cannot be fixed. Check `docker-compose.test.yml` for the same pattern.
    Amend `0011`'s out-of-scope line. Update `AGENTS.md`'s configuration section.

## Invariants and safety constraints

- **A secret value never reaches HTML, a log, an error message, or a redirect.**
  Four paths are easy to miss, and each needs its own guard: the `.bak` and temp
  files (mode `0o600` at creation, original mode preserved); the read-only "copy
  this TOML" escape hatch (**must** be `to_redacted_toml()`, or the degradation
  path becomes a secret-rendering machine — the single easiest thing here to get
  wrong); the error re-render, which echoes submitted values back and must never
  echo a `Secret` kind even on error; and `ConfigDocument`, which must not derive
  `Debug`.
- **A secret is only ever written when its source is `Inline` and the operator
  typed it.** Exactly one `write_secret` function, which refuses any other source.
  This is why the write path is `toml_edit` and not `serde::Serialize` on
  `Config`: `resolve_secrets` overwrites the inline field in place, so after load
  a tracker's `api_key` holds the value that came from the environment or from
  `/run/secrets/…`. A whole-document re-serialise would write a Kubernetes secret
  into `config.toml` in plaintext — into a file most people keep in git — and it
  would then be *ignored*, because env precedence still wins, so nobody would
  notice until it leaked.
- **`*_file` paths are display-only in the UI.** They are deployment-level by
  definition, and making them editable turns an unauthenticated page into a
  remote arbitrary-file-read primitive: point `api_key_file` at `/etc/shadow`,
  point `base_url` at your own host, press Test (0019), and the file contents
  leave as a bearer token. Display-only removes the primitive outright.
- **The UI cannot save a configuration startup would reject**, because it runs
  `Config`'s own deserialize and `problems()` before writing, and writes nothing
  if there is an error.
- **The file is never left in a state the process would refuse**: validate, then
  write.
- **Never regenerate the file from scratch.** If `toml_edit` cannot parse it,
  refuse the save with the parser's message. A file that loads but does not
  round-trip is an operator's file to fix, not ours to replace.
- **Never overwrite a good `.bak` with a bad file.** Refuse to save at all when
  the current file does not parse — it may be a hand-edit in progress.
- Nothing in `src/web/` calls `Secret::expose`.

## Likely files

- `src/config/mod.rs`, `src/config/write.rs` (split from `src/config.rs`)
- `src/web/settings/mod.rs`, `.../fields.rs`, `.../render.rs`, `.../save.rs`
- `src/web/mod.rs`, `src/web/layout.rs`, `src/web/review.rs` (`BulkForm`)
- `src/web/AGENTS.md` (new)
- `Cargo.toml`
- `README.md`, `config.example.toml`, `docker-compose.yml`, `Dockerfile`,
  `AGENTS.md`, `docs/todos/0011-configuration-and-secrets.md`

## Required tests

Unit, next to the code:

- Both drift directions (step 7).
- Every `Field` has a non-empty `label` and `help`.
- For every field, a bad value gives a field-level error, never the page fallback.
- The hidden-input bool convention, including last-value-wins.
- The path textarea: three lines, a relative line, blank lines, trailing
  whitespace.
- `save` preserves comments and key order byte-for-byte for untouched keys.
- `.bak` is written exactly once, and the original is still readable if the
  rename fails.
- Mode `0o600` on create, and the original mode preserved on replace (unix).
- An external edit between render and save is refused.
- An `Error` blocks the save and the file is unchanged; a `Warning` does not
  block.
- Render every settings page from a config whose every secret — **including
  `server.auth_token`** — is `SENTINEL-<n>`, and assert no `SENTINEL` appears in
  the HTML. Same for `to_redacted_toml()`.
- Empty secret input leaves the document byte-identical; `clear` removes the key;
  an env-sourced secret renders no input and names the variable; an error
  re-render does not echo a secret.
- A `#[test]` asserting no source file under `src/web/` contains `.expose(`. It is
  blunt, it is six lines, it passes today, and it fails the moment the invariant
  is broken.
- An unknown form key is rejected.
- Read-only degradation returns 409 and offers redacted TOML.

Integration, `tests/settings.rs`:

- **The acceptance test for the whole plan:** start from `Config::default()`,
  `POST /settings/staging` with a real path, and assert a previously parked job
  reaches `staged` — cold start to working worker, no restart.
- `POST /jobs/bulk/retry` with two `id` values works over real HTTP. **This test
  fails against `main` today.**
- Settings routes require the auth token when one is set.

## Acceptance criteria

- Every field in `Config` is reachable from `/settings`, and a test fails if one
  is added and forgotten.
- A fresh install goes from no config file to a running repair worker entirely in
  the browser.
- `config.toml` after a save is a file an operator would be happy to read:
  comments intact, only the changed keys touched.
- No secret value appears in any rendered page, log line, or written file it did
  not already come from.

## Out of scope

- Connection tests (0019).
- Browser login (0018) — independent, but it must land before this ships, or
  setting a token from the UI locks the operator out of their own browser.
- A settings-change audit table in SQLite. Tempting, and it fits "never claim
  something happened that did not", but the `.bak` plus `config.toml` usually
  being in version control covers it, and a table means a migration and a page.
- Generating `config.example.toml` from `FIELDS`. The "every field is documented
  in the example" test is most of the value for a fraction of the work.
- Any general restart-required mechanism. Two keys need it; hard-code two keys.

## Open questions

- One long page or one page per section?

  **Resolved:** one page per section, with an index. A single 45-field page cannot
  be reviewed, and a validation failure would re-render the whole thing.

- Should the settings UI be a guided multi-step wizard on first run?

  **Resolved:** no. The setup banner from 0015 lists what is unmet and links to
  the section that fixes it, which is the same guidance for a fraction of the
  code and does not need its own state machine.

- Should `library.roots` be repeated inputs rather than a textarea?

  **Resolved:** textarea. Repeated inputs need add/remove buttons and a
  round-trip per row for a list that is usually one or two entries. The cost is
  that a path containing a newline cannot be expressed, which is acceptable and
  documented.

- Is losing the operator's comments acceptable if it avoids `toml_edit`?

  **Resolved:** no, and this is the crate's justification. The UI becomes the
  primary editor but the file stays hand-editable, so silently deleting the
  comments that document every safety setting would be a real regression. The
  secrets invariant above independently rules out the serde round-trip that would
  be the only alternative.
