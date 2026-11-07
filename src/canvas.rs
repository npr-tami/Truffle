//! Spotify's Canvas: the short looping video some songs have, which Spotify's own app shows behind
//! them. Asked for from the service that app uses (`canvaz-cache`) with the logged-in session, as a
//! small protobuf written and read here: librespot's copy of the messages is cut short.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context as _;
use http::{HeaderMap, HeaderValue, Method, header};
use librespot_core::Session;
use sha2::{Digest, Sha256};

/// Canvas videos kept in the cache (each well under a megabyte or two).
const KEEP: usize = 60;
/// Larger than this is no Canvas.
const MAX_BYTES: usize = 20 * 1024 * 1024;

/// A song's Canvas, as Spotify answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canvas {
    /// The song (`spotify:track:...`).
    pub uri: String,
    /// The file (a video, or a picture), on Spotify's CDN.
    pub url: String,
    /// Spotify's kind: 0 picture, 1 video, 2 looping video, 3 looping from a random point, 4 GIF.
    pub kind: u64,
}

impl Canvas {
    pub fn is_video(&self) -> bool {
        matches!(self.kind, 1..=3)
    }
}

/// Downloads a Canvas video into `dir`, named after its address so it is fetched once, and keeps
/// the newest few there; returns where it is.
pub async fn download(url: &str, dir: &Path) -> anyhow::Result<PathBuf> {
    let name: String = Sha256::digest(url.as_bytes()).iter().take(16).map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("{name}.mp4"));
    if path.exists() {
        // Used again: newest, so it stays.
        let _ = std::fs::File::options().write(true).open(&path).and_then(|f| f.set_modified(SystemTime::now()));
        return Ok(path);
    }
    tokio::fs::create_dir_all(dir).await.context("canvas folder")?;
    let bytes = reqwest::get(url).await?.error_for_status()?.bytes().await?;
    anyhow::ensure!(bytes.len() <= MAX_BYTES, "{} bytes is too big for a Canvas", bytes.len());
    let part = path.with_extension("part");
    tokio::fs::write(&part, &bytes).await.context("canvas file")?;
    tokio::fs::rename(&part, &path).await.context("canvas file")?;
    keep_newest(dir, KEEP);
    Ok(path)
}

/// Removes all but the `keep` newest files in `dir`.
fn keep_newest(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok().filter(std::fs::Metadata::is_file)?;
            Some((meta.modified().ok()?, e.path()))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(path);
    }
}

/// The Canvas of each of `uris` that has one, in one request.
pub async fn fetch(session: &Session, uris: &[String]) -> Result<Vec<Canvas>, librespot_core::Error> {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-protobuf"));
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/protobuf"));
    let body = request(uris);
    let bytes = session.spclient().request(&Method::POST, "/canvaz-cache/v0/canvases", Some(headers), Some(&body)).await?;
    Ok(parse(&bytes))
}

/// For the self-test: which of `uris` have a Canvas, of what kind, where, and whether its file is
/// there (its first bytes, which start an MP4's).
pub async fn report(session: &Session, uris: &[String]) -> String {
    let found = match fetch(session, uris).await {
        Ok(found) => found,
        Err(e) => return format!("canvas request failed: {e}\n"),
    };
    let mut out = format!("{} of {} songs have a Canvas\n", found.len(), uris.len());
    let http = reqwest::Client::new();
    for c in &found {
        let file = match http.get(&c.url).header(reqwest::header::RANGE, "bytes=0-15").send().await {
            Ok(r) => {
                let status = r.status();
                let size = r.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(|v| v.rsplit('/').next()).unwrap_or("?").to_owned();
                let start = r.bytes().await.unwrap_or_default();
                format!("{status}, {size} bytes, mp4 {}", start.get(4..8) == Some(b"ftyp".as_slice()))
            }
            Err(e) => format!("not reachable: {e}"),
        };
        out.push_str(&format!("{} kind {} {} ({file})\n", c.uri, c.kind, c.url));
    }
    out
}

/// `EntityCanvazRequest { repeated Entity entities = 1 }`, each `Entity { string entity_uri = 1 }`.
fn request(uris: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for uri in uris {
        let mut entity = Vec::new();
        put_bytes(&mut entity, 1, uri.as_bytes());
        put_bytes(&mut out, 1, &entity);
    }
    out
}

/// `EntityCanvazResponse { repeated Canvaz canvases = 1 }`: each one's `url` (2), `type` (4) and
/// `entity_uri` (5); the rest is passed over.
fn parse(bytes: &[u8]) -> Vec<Canvas> {
    fields(bytes)
        .filter_map(|(n, v)| match (n, v) {
            (1, Value::Bytes(b)) => Some(b),
            _ => None,
        })
        .filter_map(|b| {
            let (mut uri, mut url, mut kind) = (String::new(), String::new(), 0);
            for (n, v) in fields(b) {
                match (n, v) {
                    (2, Value::Bytes(s)) => url = String::from_utf8_lossy(s).into_owned(),
                    (4, Value::Varint(k)) => kind = k,
                    (5, Value::Bytes(s)) => uri = String::from_utf8_lossy(s).into_owned(),
                    _ => {}
                }
            }
            (!url.is_empty()).then_some(Canvas { uri, url, kind })
        })
        .collect()
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A length-delimited field (a string or a message).
fn put_bytes(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    put_varint(out, u64::from(field << 3 | 2));
    put_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

/// A field's value: a number, bytes (a string or a message), or something passed over.
enum Value<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Other,
}

/// A message's fields in order (number and value), until its end or anything malformed.
fn fields(bytes: &[u8]) -> impl Iterator<Item = (u32, Value<'_>)> {
    let mut b = bytes;
    std::iter::from_fn(move || {
        let key = varint(&mut b)?;
        let value = match key & 7 {
            0 => Value::Varint(varint(&mut b)?),
            1 => {
                b = b.get(8..)?;
                Value::Other
            }
            2 => {
                let len = usize::try_from(varint(&mut b)?).ok()?;
                let data = b.get(..len)?;
                b = &b[len..];
                Value::Bytes(data)
            }
            5 => {
                b = b.get(4..)?;
                Value::Other
            }
            _ => return None,
        };
        Some(((key >> 3) as u32, value))
    })
}

fn varint(b: &mut &[u8]) -> Option<u64> {
    let mut v = 0u64;
    for i in 0..10 {
        let (&byte, rest) = b.split_first()?;
        *b = rest;
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_lists_each_song_as_an_entity() {
        let uris = vec!["spotify:track:a".to_owned(), "spotify:track:bc".to_owned()];
        let bytes = request(&uris);
        // field 1 (entities), 17 long: field 1 (entity_uri), 15 long, the URI; then the next one.
        assert_eq!(&bytes[..4], &[0x0A, 17, 0x0A, 15]);
        assert_eq!(&bytes[4..19], b"spotify:track:a");
        assert_eq!(&bytes[19..21], &[0x0A, 18]);
    }

    #[test]
    fn an_answer_gives_each_canvas_with_its_song_kind_and_file() {
        let mut canvaz = Vec::new();
        put_bytes(&mut canvaz, 1, b"id");
        put_bytes(&mut canvaz, 2, b"https://canvaz.scdn.co/upload/x.cnvs.mp4");
        put_varint(&mut canvaz, 4 << 3);
        put_varint(&mut canvaz, 2);
        put_bytes(&mut canvaz, 5, b"spotify:track:a");
        // An artist message (6) and a bool (7) in between are passed over.
        let mut artist = Vec::new();
        put_bytes(&mut artist, 2, b"Someone");
        put_bytes(&mut canvaz, 6, &artist);
        put_varint(&mut canvaz, 7 << 3);
        put_varint(&mut canvaz, 1);
        let mut answer = Vec::new();
        put_bytes(&mut answer, 1, &canvaz);
        put_varint(&mut answer, 2 << 3);
        put_varint(&mut answer, 300);
        let found = parse(&answer);
        assert_eq!(found, vec![Canvas { uri: "spotify:track:a".into(), url: "https://canvaz.scdn.co/upload/x.cnvs.mp4".into(), kind: 2 }]);
        assert!(parse(&[0x0A, 50, 1, 2]).is_empty(), "cut short: nothing, no panic");
        assert!(parse(&[]).is_empty());
    }
}
