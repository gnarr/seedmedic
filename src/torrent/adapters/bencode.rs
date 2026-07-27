//! Real `.torrent` decoding.
//!
//! Hand-written recursive-descent bencode reader rather than a dependency:
//! the one thing this module cannot get wrong is the info-hash, which must be
//! the SHA-1 of the *raw bytes* of the `info` dictionary as they appeared in
//! the file. A decode-then-re-encode pipeline risks reordering or dropping
//! keys the struct doesn't model, which silently produces the wrong hash.
//! Tracking the byte span of every parsed value as we go makes "hash the
//! original bytes" the only thing the code can do, rather than something a
//! re-encoder has to get right.
//!
//! Non-UTF-8 path components are rejected rather than lossily converted: two
//! different byte sequences could collide once forced into `String`, and
//! `SafeRelativePath` is `String`-backed throughout.
//!
//! A v2-only torrent's `info` dict has neither `length` nor `files` (it has
//! `file tree` instead), so it falls out of the single/multi-file match below
//! as a `MissingField` — an intentional rejection, not an accident of what the
//! decoder happens to support.
//!
//! `pieces` is decoded and discarded rather than kept on `TorrentMetadata`:
//! piece-hash verification is 0005's job, and the raw `.torrent` bytes are
//! already persisted on the repair job, so re-parsing them later is cheap.

use std::ops::Range;

use sha1::{Digest, Sha1};

use crate::torrent::{
    domain::{InfoHash, TorrentFile, TorrentMetadata},
    path::SafeRelativePath,
    ports::{InspectError, TorrentInspector},
};

/// A media torrent's metadata is tens of kilobytes even with a large file
/// list; 10 MiB is generous headroom while still refusing a hostile file that
/// tries to exhaust memory before a single field has been read.
const MAX_TORRENT_BYTES: usize = 10 * 1024 * 1024;

/// Real torrent structures nest a handful of levels deep (dict -> info ->
/// files -> file dict -> path list). 32 comfortably covers that with room to
/// spare, while bounding recursion so a hostile file can't blow the stack.
const MAX_DEPTH: usize = 32;

/// Larger than any real media torrent's file count; bounds the work done
/// decoding a `files` list so a hostile torrent can't force unbounded
/// allocation or iteration.
const MAX_FILES: usize = 10_000;

#[derive(Clone, Copy, Debug, Default)]
pub struct BencodeInspector;

impl TorrentInspector for BencodeInspector {
    fn inspect(&self, bytes: &[u8]) -> Result<TorrentMetadata, InspectError> {
        if bytes.len() > MAX_TORRENT_BYTES {
            return Err(InspectError::Malformed(format!(
                "torrent is {} bytes, limit is {MAX_TORRENT_BYTES}",
                bytes.len()
            )));
        }

        let root = parse(bytes).map_err(InspectError::Malformed)?;
        let entries = root.value.as_dict().map_err(InspectError::Malformed)?;

        let info_node = find(entries, b"info").ok_or(InspectError::MissingField("info"))?;
        let info_entries = info_node.value.as_dict().map_err(InspectError::Malformed)?;
        let info_hash = InfoHash::from_bytes(Sha1::digest(&bytes[info_node.span.clone()]).into());

        let name = decode_path_component(
            find(info_entries, b"name").ok_or(InspectError::MissingField("info.name"))?,
        )?;
        let name = SafeRelativePath::from_components([name])?;

        let piece_length = as_length(
            find(info_entries, b"piece length")
                .ok_or(InspectError::MissingField("info.piece length"))?,
        )?;

        let files = match (find(info_entries, b"length"), find(info_entries, b"files")) {
            (Some(length_node), None) => vec![TorrentFile {
                path: name.clone(),
                length: as_length(length_node)?,
            }],
            (None, Some(files_node)) => decode_files(files_node, &name)?,
            (Some(_), Some(_)) => {
                return Err(InspectError::Malformed(
                    "info contains both `length` and `files`".to_owned(),
                ));
            }
            (None, None) => return Err(InspectError::MissingField("info.length or info.files")),
        };

        Ok(TorrentMetadata {
            info_hash,
            name,
            piece_length,
            files,
        })
    }
}

fn decode_files(
    node: &Node,
    torrent_name: &SafeRelativePath,
) -> Result<Vec<TorrentFile>, InspectError> {
    let entries = node.value.as_list().map_err(InspectError::Malformed)?;
    if entries.len() > MAX_FILES {
        return Err(InspectError::Malformed(format!(
            "torrent declares {} files, limit is {MAX_FILES}",
            entries.len()
        )));
    }

    entries
        .iter()
        .map(|file_node| decode_file(file_node, torrent_name))
        .collect()
}

fn decode_file(node: &Node, torrent_name: &SafeRelativePath) -> Result<TorrentFile, InspectError> {
    let entries = node.value.as_dict().map_err(InspectError::Malformed)?;

    let length = as_length(
        find(entries, b"length").ok_or(InspectError::MissingField("info.files[].length"))?,
    )?;

    let path_node =
        find(entries, b"path").ok_or(InspectError::MissingField("info.files[].path"))?;
    let components = path_node.value.as_list().map_err(InspectError::Malformed)?;
    if components.is_empty() {
        return Err(InspectError::MissingField("info.files[].path"));
    }
    let components = components
        .iter()
        .map(decode_path_component)
        .collect::<Result<Vec<_>, _>>()?;

    let path = SafeRelativePath::from_components(components)?.under(torrent_name);
    Ok(TorrentFile { path, length })
}

fn decode_path_component(node: &Node) -> Result<String, InspectError> {
    let bytes = node.value.as_bytes().map_err(InspectError::Malformed)?;
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| InspectError::Malformed("path component is not valid UTF-8".to_owned()))
}

fn as_length(node: &Node) -> Result<u64, InspectError> {
    match node.value {
        Value::Int(value) if value >= 0 => Ok(value as u64),
        Value::Int(value) => Err(InspectError::Malformed(format!(
            "length must not be negative, got {value}"
        ))),
        _ => Err(InspectError::Malformed("expected an integer".to_owned())),
    }
}

fn find<'a>(entries: &'a [(Vec<u8>, Node)], key: &[u8]) -> Option<&'a Node> {
    entries.iter().find(|(k, _)| k == key).map(|(_, node)| node)
}

// --- Minimal recursive-descent bencode reader ---------------------------
//
// Every parsed value keeps the byte range it came from (`Node::span`), which
// is what lets the info-hash be computed over the exact original bytes.

#[derive(Debug)]
struct Node {
    value: Value,
    span: Range<usize>,
}

#[derive(Debug)]
enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Node>),
    Dict(Vec<(Vec<u8>, Node)>),
}

impl Value {
    fn as_dict(&self) -> Result<&[(Vec<u8>, Node)], String> {
        match self {
            Value::Dict(entries) => Ok(entries),
            _ => Err("expected a dictionary".to_owned()),
        }
    }

    fn as_list(&self) -> Result<&[Node], String> {
        match self {
            Value::List(items) => Ok(items),
            _ => Err("expected a list".to_owned()),
        }
    }

    fn as_bytes(&self) -> Result<&[u8], String> {
        match self {
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err("expected a byte string".to_owned()),
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Node, String> {
    Parser { bytes, pos: 0 }.parse_node(0)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Result<u8, String> {
        self.bytes
            .get(self.pos)
            .copied()
            .ok_or_else(|| "unexpected end of input".to_owned())
    }

    fn advance(&mut self) -> Result<u8, String> {
        let byte = self.peek()?;
        self.pos += 1;
        Ok(byte)
    }

    fn parse_node(&mut self, depth: usize) -> Result<Node, String> {
        if depth > MAX_DEPTH {
            return Err(format!("nesting exceeds limit of {MAX_DEPTH}"));
        }

        let start = self.pos;
        let value = match self.peek()? {
            b'i' => self.parse_int()?,
            b'l' => self.parse_list(depth)?,
            b'd' => self.parse_dict(depth)?,
            b'0'..=b'9' => self.parse_bytes()?,
            other => return Err(format!("unexpected byte {other:#04x} at offset {start}")),
        };

        Ok(Node {
            value,
            span: start..self.pos,
        })
    }

    fn parse_int(&mut self) -> Result<Value, String> {
        self.advance()?; // 'i'
        let digits = self.take_until(b'e')?;
        let text = std::str::from_utf8(digits).map_err(|_| "integer is not ASCII".to_owned())?;
        if text.is_empty()
            || text == "-"
            || text.starts_with("-0")
            || (text.len() > 1 && text.starts_with('0'))
        {
            return Err(format!("malformed integer {text:?}"));
        }
        text.parse::<i64>()
            .map(Value::Int)
            .map_err(|error| format!("malformed integer {text:?}: {error}"))
    }

    fn parse_bytes(&mut self) -> Result<Value, String> {
        let digits = self.take_until(b':')?;
        let text = std::str::from_utf8(digits).map_err(|_| "length is not ASCII".to_owned())?;
        if text.len() > 1 && text.starts_with('0') {
            return Err(format!("byte string length has a leading zero: {text:?}"));
        }
        let len: usize = text
            .parse()
            .map_err(|error| format!("malformed byte string length {text:?}: {error}"))?;

        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| "byte string length overflows".to_owned())?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| "byte string runs past end of input".to_owned())?;
        self.pos = end;
        Ok(Value::Bytes(slice.to_vec()))
    }

    fn parse_list(&mut self, depth: usize) -> Result<Value, String> {
        self.advance()?; // 'l'
        let mut items = Vec::new();
        loop {
            if self.peek()? == b'e' {
                self.advance()?;
                return Ok(Value::List(items));
            }
            items.push(self.parse_node(depth + 1)?);
        }
    }

    fn parse_dict(&mut self, depth: usize) -> Result<Value, String> {
        self.advance()?; // 'd'
        let mut entries = Vec::new();
        loop {
            if self.peek()? == b'e' {
                self.advance()?;
                return Ok(Value::Dict(entries));
            }
            let key = match self.parse_bytes()? {
                Value::Bytes(key) => key,
                _ => unreachable!("parse_bytes only ever returns Value::Bytes"),
            };
            entries.push((key, self.parse_node(depth + 1)?));
        }
    }

    /// Reads ASCII digit bytes (with an optional leading `-`) up to and
    /// including `delimiter`, returning the bytes before it.
    fn take_until(&mut self, delimiter: u8) -> Result<&'a [u8], String> {
        let bytes = self.bytes;
        let start = self.pos;
        loop {
            match self.advance()? {
                byte if byte == delimiter => return Ok(&bytes[start..self.pos - 1]),
                b'-' | b'0'..=b'9' => {}
                other => return Err(format!("unexpected byte {other:#04x} in a number")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real `.torrent` fixtures. Both declare a `private` key that
    // `TorrentMetadata` does not model, so a correct info-hash on these is
    // also the regression test for the re-encoding trap: hashing a
    // re-encode of the decoded struct would silently drop `private` and
    // produce a different hash.
    const SINGLE_FILE_TORRENT: &[u8] =
        include_bytes!("../../../tests/fixtures/single_file.torrent");
    const MULTI_FILE_TORRENT: &[u8] = include_bytes!("../../../tests/fixtures/multi_file.torrent");

    // Independently verified with `sha1sum` on the raw `info` dict bytes,
    // not through this decoder.
    const SINGLE_FILE_INFO_HASH: &str = "907f6d65097ab76394efdc49f6f9b1b81036e4fc";
    const MULTI_FILE_INFO_HASH: &str = "67319854c873e8e7738a75b2da38884f9cbbe6ab";

    fn bstr(bytes: &[u8]) -> Vec<u8> {
        let mut v = format!("{}:", bytes.len()).into_bytes();
        v.extend_from_slice(bytes);
        v
    }

    fn int(n: i64) -> Vec<u8> {
        format!("i{n}e").into_bytes()
    }

    fn dict(entries: &[(&[u8], Vec<u8>)]) -> Vec<u8> {
        let mut v = b"d".to_vec();
        for (key, value) in entries {
            v.extend(bstr(key));
            v.extend_from_slice(value);
        }
        v.push(b'e');
        v
    }

    fn list(items: &[Vec<u8>]) -> Vec<u8> {
        let mut v = b"l".to_vec();
        for item in items {
            v.extend_from_slice(item);
        }
        v.push(b'e');
        v
    }

    fn torrent_with_info(info: Vec<u8>) -> Vec<u8> {
        dict(&[
            (b"announce", bstr(b"http://tracker.example/announce")),
            (b"info", info),
        ])
    }

    fn file_entry(length: i64, path_components: &[&[u8]]) -> Vec<u8> {
        dict(&[
            (b"length", int(length)),
            (
                b"path",
                list(&path_components.iter().map(|c| bstr(c)).collect::<Vec<_>>()),
            ),
        ])
    }

    #[test]
    fn decodes_single_file_fixture_with_correct_info_hash() {
        let metadata = BencodeInspector
            .inspect(SINGLE_FILE_TORRENT)
            .expect("valid torrent");
        assert_eq!(
            metadata.info_hash,
            InfoHash::parse_hex(SINGLE_FILE_INFO_HASH).expect("valid hex")
        );
        assert_eq!(metadata.name.as_str(), "movie.mkv");
        assert_eq!(metadata.piece_length, 262_144);
        assert_eq!(metadata.file_count(), 1);
        assert_eq!(metadata.total_length(), 1_048_576);
        assert_eq!(metadata.files[0].path.as_str(), "movie.mkv");
        assert_eq!(metadata.files[0].length, 1_048_576);
    }

    #[test]
    fn decodes_multi_file_fixture_with_correct_info_hash_and_prefixed_paths() {
        let metadata = BencodeInspector
            .inspect(MULTI_FILE_TORRENT)
            .expect("valid torrent");
        assert_eq!(
            metadata.info_hash,
            InfoHash::parse_hex(MULTI_FILE_INFO_HASH).expect("valid hex")
        );
        assert_eq!(metadata.name.as_str(), "Show S01");
        assert_eq!(metadata.piece_length, 16_384);
        assert_eq!(metadata.file_count(), 2);
        assert_eq!(metadata.total_length(), 300);
        assert_eq!(metadata.files[0].path.as_str(), "Show S01/e01.mkv");
        assert_eq!(metadata.files[0].length, 100);
        assert_eq!(metadata.files[1].path.as_str(), "Show S01/e02.mkv");
        assert_eq!(metadata.files[1].length, 200);
    }

    #[test]
    fn rejects_parent_traversal_path() {
        let info = dict(&[
            (b"files", list(&[file_entry(1, &[b".."])])),
            (b"name", bstr(b"Show")),
            (b"piece length", int(16_384)),
        ]);
        assert!(matches!(
            BencodeInspector.inspect(&torrent_with_info(info)),
            Err(InspectError::UnsafePath(_))
        ));
    }

    #[test]
    fn rejects_absolute_path_component() {
        let info = dict(&[
            (b"files", list(&[file_entry(1, &[b"/etc/passwd"])])),
            (b"name", bstr(b"Show")),
            (b"piece length", int(16_384)),
        ]);
        assert!(matches!(
            BencodeInspector.inspect(&torrent_with_info(info)),
            Err(InspectError::UnsafePath(_))
        ));
    }

    #[test]
    fn rejects_nul_byte_in_path_component() {
        let info = dict(&[
            (b"files", list(&[file_entry(1, &[b"ep\0.mkv"])])),
            (b"name", bstr(b"Show")),
            (b"piece length", int(16_384)),
        ]);
        assert!(matches!(
            BencodeInspector.inspect(&torrent_with_info(info)),
            Err(InspectError::UnsafePath(_))
        ));
    }

    #[test]
    fn rejects_non_utf8_path_component() {
        let info = dict(&[
            (b"files", list(&[file_entry(1, &[&[0xff, 0xfe]])])),
            (b"name", bstr(b"Show")),
            (b"piece length", int(16_384)),
        ]);
        assert!(matches!(
            BencodeInspector.inspect(&torrent_with_info(info)),
            Err(InspectError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_non_utf8_name() {
        let info = dict(&[
            (b"length", int(1)),
            (b"name", bstr(&[0xff, 0xfe])),
            (b"piece length", int(16_384)),
        ]);
        assert!(matches!(
            BencodeInspector.inspect(&torrent_with_info(info)),
            Err(InspectError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_missing_name() {
        let info = dict(&[(b"length", int(1)), (b"piece length", int(16_384))]);
        assert_eq!(
            BencodeInspector.inspect(&torrent_with_info(info)),
            Err(InspectError::MissingField("info.name"))
        );
    }

    #[test]
    fn rejects_v2_only_torrent() {
        // No `length` and no `files`: a v2-only `info` dict has `file tree`
        // instead, which we deliberately do not understand.
        let info = dict(&[
            (b"file tree", dict(&[])),
            (b"meta version", int(2)),
            (b"name", bstr(b"Show")),
            (b"piece length", int(16_384)),
        ]);
        assert_eq!(
            BencodeInspector.inspect(&torrent_with_info(info)),
            Err(InspectError::MissingField("info.length or info.files"))
        );
    }

    #[test]
    fn rejects_ambiguous_length_and_files() {
        let info = dict(&[
            (b"files", list(&[])),
            (b"length", int(1)),
            (b"name", bstr(b"Show")),
            (b"piece length", int(16_384)),
        ]);
        assert!(matches!(
            BencodeInspector.inspect(&torrent_with_info(info)),
            Err(InspectError::Malformed(_))
        ));
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(
            BencodeInspector
                .inspect(&SINGLE_FILE_TORRENT[..SINGLE_FILE_TORRENT.len() - 10])
                .is_err()
        );
        assert!(BencodeInspector.inspect(&SINGLE_FILE_TORRENT[..1]).is_err());
    }

    #[test]
    fn rejects_empty_input() {
        assert!(BencodeInspector.inspect(&[]).is_err());
    }

    #[test]
    fn rejects_a_megabyte_of_dict_starts_without_panicking() {
        let bytes = vec![b'd'; 1024 * 1024];
        assert!(BencodeInspector.inspect(&bytes).is_err());
    }

    #[test]
    fn rejects_input_over_the_size_cap() {
        let bytes = vec![b'0'; MAX_TORRENT_BYTES + 1];
        assert!(matches!(
            BencodeInspector.inspect(&bytes),
            Err(InspectError::Malformed(_))
        ));
    }

    #[test]
    fn does_not_panic_on_random_bytes() {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = (next() % 64) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| (next() % 256) as u8).collect();
            let _ = BencodeInspector.inspect(&bytes);
        }
    }
}
