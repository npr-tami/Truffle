//! Minimal hand-written protobuf codec for `collection2v2.proto`
//! (librespot-protocol-0.8.0/proto/collection2v2.proto, package spotify.collection.proto.v2).
//! librespot-protocol does not compile that file, so the few messages Truffle needs for the
//! Liked Songs collection are encoded/decoded here. Proto3: default values are not written.
//!
//! ```text
//! PageRequest   { string username = 1; string set = 2; string pagination_token = 3; int32 limit = 4; }
//! CollectionItem{ string uri = 1; int32 added_at = 2; bool is_removed = 3; optional string context_uri = 4; }
//! PageResponse  { repeated CollectionItem items = 1; string next_page_token = 2; string sync_token = 3; }
//! DeltaRequest  { string username = 1; string set = 2; string last_sync_token = 3; }
//! DeltaResponse { bool delta_update_possible = 1; repeated CollectionItem items = 2; string sync_token = 3; }
//! WriteRequest  { string username = 1; string set = 2; repeated CollectionItem items = 3; string client_update_id = 4; }
//! ```

const WIRE_VARINT: u8 = 0;
const WIRE_FIXED64: u8 = 1;
const WIRE_LEN: u8 = 2;
const WIRE_FIXED32: u8 = 5;

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

fn put_tag(buf: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(buf, ((field as u64) << 3) | wire as u64);
}

fn put_bytes(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_tag(buf, field, WIRE_LEN);
    put_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

fn put_str(buf: &mut Vec<u8>, field: u32, s: &str) {
    if !s.is_empty() {
        put_bytes(buf, field, s.as_bytes());
    }
}

fn put_int32(buf: &mut Vec<u8>, field: u32, v: i32) {
    if v != 0 {
        put_tag(buf, field, WIRE_VARINT);
        // Negative int32 values are sign-extended to 64 bits on the wire.
        put_varint(buf, v as i64 as u64);
    }
}

fn put_bool(buf: &mut Vec<u8>, field: u32, v: bool) {
    if v {
        put_tag(buf, field, WIRE_VARINT);
        put_varint(buf, 1);
    }
}

enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed,
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos).ok_or("truncated varint")?;
            self.pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err("varint too long".into())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or("truncated field")?;
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    /// Next (field number, value), or `None` at the end of the buffer.
    fn field(&mut self) -> Result<Option<(u32, Value<'a>)>, String> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let tag = self.varint()?;
        let field = (tag >> 3) as u32;
        let value = match (tag & 7) as u8 {
            WIRE_VARINT => Value::Varint(self.varint()?),
            WIRE_FIXED64 => {
                self.take(8)?;
                Value::Fixed
            }
            WIRE_LEN => {
                let len = self.varint()? as usize;
                Value::Bytes(self.take(len)?)
            }
            WIRE_FIXED32 => {
                self.take(4)?;
                Value::Fixed
            }
            other => return Err(format!("unsupported wire type {other}")),
        };
        Ok(Some((field, value)))
    }
}

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollectionItem {
    pub uri: String,
    /// Seconds since the Unix epoch.
    pub added_at: i32,
    pub is_removed: bool,
}

impl CollectionItem {
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.uri.len() + 12);
        put_str(&mut buf, 1, &self.uri);
        put_int32(&mut buf, 2, self.added_at);
        put_bool(&mut buf, 3, self.is_removed);
        buf
    }

    fn decode(bytes: &[u8]) -> Result<Self, String> {
        let mut item = Self::default();
        let mut r = Reader::new(bytes);
        while let Some((field, value)) = r.field()? {
            match (field, value) {
                (1, Value::Bytes(b)) => item.uri = utf8(b),
                (2, Value::Varint(v)) => item.added_at = v as i64 as i32,
                (3, Value::Varint(v)) => item.is_removed = v != 0,
                _ => {}
            }
        }
        Ok(item)
    }
}

pub fn encode_page_request(username: &str, set: &str, pagination_token: &str, limit: i32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    put_str(&mut buf, 1, username);
    put_str(&mut buf, 2, set);
    put_str(&mut buf, 3, pagination_token);
    put_int32(&mut buf, 4, limit);
    buf
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PageResponse {
    pub items: Vec<CollectionItem>,
    pub next_page_token: String,
    pub sync_token: String,
}

pub fn decode_page_response(bytes: &[u8]) -> Result<PageResponse, String> {
    let mut page = PageResponse::default();
    let mut r = Reader::new(bytes);
    while let Some((field, value)) = r.field()? {
        match (field, value) {
            (1, Value::Bytes(b)) => page.items.push(CollectionItem::decode(b)?),
            (2, Value::Bytes(b)) => page.next_page_token = utf8(b),
            (3, Value::Bytes(b)) => page.sync_token = utf8(b),
            _ => {}
        }
    }
    Ok(page)
}

pub fn encode_delta_request(username: &str, set: &str, last_sync_token: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + last_sync_token.len());
    put_str(&mut buf, 1, username);
    put_str(&mut buf, 2, set);
    put_str(&mut buf, 3, last_sync_token);
    buf
}

/// The changes to a set since a sync token. `delta_update_possible` is false when the server can't
/// list them (the token is too old): the whole set has to be read again.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeltaResponse {
    pub delta_update_possible: bool,
    pub items: Vec<CollectionItem>,
    pub sync_token: String,
}

pub fn decode_delta_response(bytes: &[u8]) -> Result<DeltaResponse, String> {
    let mut delta = DeltaResponse::default();
    let mut r = Reader::new(bytes);
    while let Some((field, value)) = r.field()? {
        match (field, value) {
            (1, Value::Varint(v)) => delta.delta_update_possible = v != 0,
            (2, Value::Bytes(b)) => delta.items.push(CollectionItem::decode(b)?),
            (3, Value::Bytes(b)) => delta.sync_token = utf8(b),
            _ => {}
        }
    }
    Ok(delta)
}

pub fn encode_write_request(username: &str, set: &str, items: &[CollectionItem], client_update_id: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + items.len() * 48);
    put_str(&mut buf, 1, username);
    put_str(&mut buf, 2, set);
    for item in items {
        put_bytes(&mut buf, 3, &item.encode());
    }
    put_str(&mut buf, 4, client_update_id);
    buf
}

/// Tests: a server's reply.
#[cfg(test)]
pub fn encode_page_response(page: &PageResponse) -> Vec<u8> {
    let mut buf = Vec::new();
    for item in &page.items {
        put_bytes(&mut buf, 1, &item.encode());
    }
    put_str(&mut buf, 2, &page.next_page_token);
    put_str(&mut buf, 3, &page.sync_token);
    buf
}

/// Tests: a server's reply.
#[cfg(test)]
pub fn encode_delta_response(delta: &DeltaResponse) -> Vec<u8> {
    let mut buf = Vec::new();
    put_bool(&mut buf, 1, delta.delta_update_possible);
    for item in &delta.items {
        put_bytes(&mut buf, 2, &item.encode());
    }
    put_str(&mut buf, 3, &delta.sync_token);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_response_round_trip() {
        let page = PageResponse {
            items: vec![
                CollectionItem { uri: "spotify:track:a".into(), added_at: 1_700_000_000, is_removed: false },
                CollectionItem { uri: "spotify:track:b".into(), added_at: -5, is_removed: true },
            ],
            next_page_token: "next".into(),
            sync_token: "sync".into(),
        };
        let bytes = encode_page_response(&page);
        assert_eq!(decode_page_response(&bytes).unwrap(), page);
    }

    #[test]
    fn page_request_known_bytes() {
        // username="u" (field 1), set="collection" (field 2), limit=300 (field 4)
        let bytes = encode_page_request("u", "collection", "", 300);
        let mut expected = vec![0x0a, 0x01, b'u', 0x12, 10];
        expected.extend_from_slice(b"collection");
        expected.extend_from_slice(&[0x20, 0xac, 0x02]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn delta_round_trip() {
        // last_sync_token="t" (field 3) after username and set.
        let bytes = encode_delta_request("u", "collection", "t");
        let mut expected = vec![0x0a, 0x01, b'u', 0x12, 10];
        expected.extend_from_slice(b"collection");
        expected.extend_from_slice(&[0x1a, 0x01, b't']);
        assert_eq!(bytes, expected);

        let delta = DeltaResponse {
            delta_update_possible: true,
            items: vec![
                CollectionItem { uri: "spotify:track:new".into(), added_at: 1_800_000_000, is_removed: false },
                CollectionItem { uri: "spotify:track:gone".into(), added_at: 0, is_removed: true },
            ],
            sync_token: "s2".into(),
        };
        assert_eq!(decode_delta_response(&encode_delta_response(&delta)).unwrap(), delta);
        // Nothing written: no delta possible, no items.
        assert_eq!(decode_delta_response(&[]).unwrap(), DeltaResponse::default());
        assert!(decode_delta_response(&[0x12, 0x09, 0x0a]).is_err());
    }

    #[test]
    fn write_request_contains_items() {
        let items = [CollectionItem { uri: "spotify:track:x".into(), added_at: 1, is_removed: true }];
        let bytes = encode_write_request("u", "collection", &items, "id");
        let mut r = Reader::new(&bytes);
        let mut fields = Vec::new();
        while let Some((f, v)) = r.field().unwrap() {
            if let (3, Value::Bytes(b)) = (f, &v) {
                assert_eq!(CollectionItem::decode(b).unwrap(), items[0]);
            }
            fields.push(f);
        }
        assert_eq!(fields, vec![1, 2, 3, 4]);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode_page_response(&[0x0a, 0x05, 0x01]).is_err());
        assert!(decode_page_response(&[0xff; 12]).is_err());
        assert!(decode_page_response(&[0x0b]).is_err());
        assert_eq!(decode_page_response(&[]).unwrap(), PageResponse::default());
    }
}
