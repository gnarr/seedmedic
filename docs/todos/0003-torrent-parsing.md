# 0003 — Bencode decoding and info-hash derivation

**Status:** Not started
**Depends on:** nothing
**Blocks:** 0004, 0005, 0006

## Problem

`torrent::adapters::bencode::BencodeInspector` fails every call, so nothing can
read a real `.torrent`. Until it works, only the fake tracker's JSON torrents can
be repaired.

This is small, pure, self-contained work with a nasty detail in the middle: the
info-hash is the SHA-1 of the **raw bytes of the `info` dictionary as they appear
in the file**, not of a re-encoding. Decoding into a struct and re-encoding it
loses unknown keys and can reorder — either produces a hash that is subtly wrong
and will not match anything in the swarm.

## Architectural context

`torrent::TorrentInspector` is a synchronous port: input is tens of kilobytes,
the work is pure CPU. `TorrentMetadata` deliberately keeps only what a repair
needs — info-hash, name, piece length, file list — so there is nothing extra to
keep correct.

Every path goes through `SafeRelativePath` at construction, so a torrent with a
hostile path fails to parse rather than producing a value somebody has to
remember to validate later. That is the property to preserve.

## Expected behaviour

- Single-file and multi-file torrents both decode. A single-file torrent
  produces one `TorrentFile` whose path is the torrent name.
- The info-hash matches what a BitTorrent client computes for the same file.
- Every path component is validated; a torrent containing `..`, an absolute
  path, or a NUL byte returns `InspectError::UnsafePath` and no partial result.
- Missing required fields return `MissingField`, not a default.
- Malformed bencode returns `Malformed` with something diagnosable.
- Nothing panics, on any input, including truncated and adversarial files.

## Implementation steps

1. **Choose the decoder.** The requirement is access to the raw byte range of
   the `info` dictionary. Options:
   - `bendy` — has a streaming decoder that can hand back raw object bytes.
   - `serde_bencode` plus a small hand-written scan that locates `4:info` and
     the matching dictionary end, slicing the original buffer.
   - A hand-written bencode reader — perhaps 150 lines, no dependency, total
     control, and one more thing to own.

   Pick one and record why. Prefer whichever makes "hash the original bytes"
   obviously correct rather than incidentally correct.

2. **Decode the metadata.** `info.name`, `info.piece length`, and either
   `info.length` (single file) or `info.files[]` with `length` and `path`.
   Torrent path components are byte strings, not necessarily UTF-8: decide
   whether to reject non-UTF-8 (simplest, and `SafeRelativePath` is `String`
   backed) or to lossily convert (dangerous — two different components could
   collide). Rejecting is the safer default; record the decision.

3. **Derive the info-hash.** SHA-1 over the raw `info` bytes. Add `sha1` to
   `Cargo.toml`. Guard it with a test against a known-good fixture.

4. **Validate paths.** Build each `SafeRelativePath` with `from_components`,
   prefixed with the torrent name for multi-file torrents. Any rejection fails
   the whole parse.

5. **Bound the input.** Refuse absurd files early — a size cap on the buffer, a
   cap on file count, a cap on nesting depth — so a hostile `.torrent` cannot
   exhaust memory. State the limits as named constants.

6. **BitTorrent v2.** Decide what to do with a v2-only torrent (`meta version
   2`, `file tree`). Failing with a clear `MissingField`-style error is
   acceptable for now; silently mis-parsing it is not.

7. **Delete the stub** and the `const TODO`.

## Invariants and safety constraints

- The info-hash is over the original bytes. Never over a re-encoding.
- A rejected path fails the whole torrent. No partial metadata escapes.
- No panics, no unbounded allocation, no recursion without a depth limit.
- `SafeRelativePath` remains the only way a torrent path becomes a filesystem
  path.

## Likely files

- `src/torrent/adapters/bencode.rs`
- `src/torrent/domain.rs` (only if v2 needs a representation)
- `src/torrent/path.rs` (only if a rejection case is missing)
- `Cargo.toml`
- `tests/fixtures/` (new)

## Required tests

- A real single-file `.torrent` fixture: correct info-hash, one file, right size.
- A real multi-file `.torrent` fixture: correct info-hash, all files, paths
  prefixed with the torrent name.
- The info-hash matches a value computed by an independent tool, recorded in the
  test.
- A torrent whose `info` dict contains a key the struct does not model still
  hashes correctly — this is the regression test for the re-encoding trap.
- Path traversal, absolute paths, and NUL bytes are rejected.
- Truncated input, empty input, and a 1 MiB file of `d` all return an error
  without panicking.
- Non-UTF-8 path components behave as decided.

## Acceptance criteria

- Real `.torrent` files from at least two trackers decode with correct
  info-hashes.
- No input causes a panic. A quick fuzz-style loop over random byte strings in a
  test is cheap insurance.
- The stub and its `NotImplemented` are gone.
- `torrent::adapters::fake` still works — the fake tracker path must keep
  running, since the tests depend on it.

## Out of scope

- Writing `.torrent` files.
- Magnet links, DHT, trackers-in-the-torrent, or anything about the swarm.
- Piece-hash verification — that is 0005, which will need `piece_length` and the
  `pieces` array. Consider whether to store `pieces` on `TorrentMetadata` now or
  re-parse later; either is fine, but say which.

## Open questions

- Which decoder, and does it give the raw `info` bytes without a re-encode?
- Reject or lossily convert non-UTF-8 path components?
- Does `TorrentMetadata` grow a `pieces: Vec<[u8; 20]>` now for 0005, or does
  0005 re-parse the stored bytes? Storing it makes the type bigger for a use
  case that does not exist yet; re-parsing is cheap and the bytes are already
  persisted.
- What is a sensible cap on file count for a media torrent?
