//! Timed lyrics from LRCLIB (lrclib.net, free and open, used by many players), for songs whose lyrics
//! Spotify has without times or not at all: asked for by the song's title, artist, album and length.
//! Its `[mm:ss.xx]` lines become Truffle's lyric lines.

use std::time::Duration;

use serde::Deserialize;

use crate::model::{LyricLine, TrackInfo};

const USER_AGENT: &str = concat!("Truffle/", env!("CARGO_PKG_VERSION"), " (Windows; personal player)");
const API: &str = "https://lrclib.net/api";
/// A found song's length may differ this much from Spotify's.
const LENGTH_SLACK_SECS: f64 = 3.0;

/// What LRCLIB has for a song: timed lines, lines without times, or nothing.
#[derive(Debug, PartialEq)]
pub enum Found {
    Timed(Vec<LyricLine>),
    Plain(Vec<LyricLine>),
    Nothing,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    duration: Option<f64>,
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
}

/// LRCLIB's lyrics for `track`: the exact match first (title, artist, album and length), then a search
/// by title and artist, taking the first with timed lines and about the same length.
pub async fn find(track: &TrackInfo) -> Found {
    let Some(artist) = track.artists.first().map(|a| a.name.clone()) else { return Found::Nothing };
    let Ok(http) = reqwest::Client::builder().user_agent(USER_AGENT).timeout(Duration::from_secs(10)).build() else { return Found::Nothing };
    let secs = f64::from(track.duration_ms) / 1000.0;
    let exact = http
        .get(format!("{API}/get"))
        .query(&[("track_name", track.name.as_str()), ("artist_name", artist.as_str()), ("album_name", track.album.as_str()), ("duration", &format!("{}", secs.round()))])
        .send()
        .await;
    if let Ok(r) = exact
        && r.status().is_success()
        && let Ok(record) = r.json::<Record>().await
        && let found @ (Found::Timed(_) | Found::Plain(_)) = pick(vec![record], secs)
    {
        return found;
    }
    let search = http.get(format!("{API}/search")).query(&[("track_name", track.name.as_str()), ("artist_name", artist.as_str())]).send().await;
    match search {
        Ok(r) if r.status().is_success() => match r.json::<Vec<Record>>().await {
            Ok(records) => pick(records, secs),
            Err(e) => {
                log::debug!("lrclib search for {}: {e}", track.uri);
                Found::Nothing
            }
        },
        Ok(_) => Found::Nothing,
        Err(e) => {
            log::debug!("lrclib for {}: {e}", track.uri);
            Found::Nothing
        }
    }
}

/// The best of `records` for a song `secs` long: the first with timed lines and about that length,
/// else the first with plain lines and about that length.
fn pick(records: Vec<Record>, secs: f64) -> Found {
    let fits = |r: &Record| r.duration.is_none_or(|d| (d - secs).abs() <= LENGTH_SLACK_SECS);
    let records: Vec<Record> = records.into_iter().filter(|r| fits(r)).collect();
    if let Some(lines) = records.iter().filter_map(|r| r.synced_lyrics.as_deref()).map(parse_lrc).find(|l| !l.is_empty()) {
        return Found::Timed(lines);
    }
    records
        .iter()
        .filter_map(|r| r.plain_lyrics.as_deref())
        .map(|text| text.lines().map(str::trim).filter(|l| !l.is_empty()).map(|words| LyricLine { start_ms: 0, words: words.to_owned() }).collect::<Vec<_>>())
        .find(|l| !l.is_empty())
        .map_or(Found::Nothing, Found::Plain)
}

/// LRC text as lines: each `[mm:ss.xx]` time (a line may have several) with its words, in time
/// order; tags such as `[ar: ...]` and empty lines are left out.
fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut times = Vec::new();
        while let Some(inner) = rest.strip_prefix('[') {
            let Some(end) = inner.find(']') else { break };
            let Some(ms) = parse_time(&inner[..end]) else { break };
            times.push(ms);
            rest = inner[end + 1..].trim_start();
        }
        let words = rest.trim();
        if words.is_empty() {
            continue;
        }
        lines.extend(times.into_iter().map(|start_ms| LyricLine { start_ms, words: words.to_owned() }));
    }
    lines.sort_by_key(|l| l.start_ms);
    lines
}

/// `mm:ss.xx` (or `mm:ss`) in milliseconds.
fn parse_time(tag: &str) -> Option<u32> {
    let (minutes, seconds) = tag.split_once(':')?;
    let minutes: u32 = minutes.trim().parse().ok()?;
    let seconds: f64 = seconds.trim().parse().ok()?;
    (seconds >= 0.0).then(|| minutes * 60_000 + (seconds * 1000.0).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(duration: f64, synced: Option<&str>, plain: Option<&str>) -> Record {
        Record { duration: Some(duration), synced_lyrics: synced.map(str::to_owned), plain_lyrics: plain.map(str::to_owned) }
    }

    #[test]
    fn lrc_lines_come_with_their_times_in_order() {
        let lines = parse_lrc("[ar: Someone]\n[00:12.34] First line\n[01:02.5][00:20.00]Again\n\n[00:30.00]\n[bad] no time\nno tag");
        let got: Vec<(u32, &str)> = lines.iter().map(|l| (l.start_ms, l.words.as_str())).collect();
        assert_eq!(got, vec![(12_340, "First line"), (20_000, "Again"), (62_500, "Again")]);
    }

    #[test]
    fn the_first_timed_lyrics_of_about_the_same_length_are_picked() {
        let records = vec![
            record(300.0, Some("[00:01.00] Other song"), None),
            record(181.0, None, Some("Plain only")),
            record(179.0, Some("[00:05.00] This one"), None),
        ];
        assert_eq!(pick(records, 180.0), Found::Timed(vec![LyricLine { start_ms: 5_000, words: "This one".into() }]));
        // No timed lines of that length: the plain ones, without times.
        let records = vec![record(180.0, None, Some("One\n\nTwo"))];
        assert_eq!(pick(records, 180.0), Found::Plain(vec![LyricLine { start_ms: 0, words: "One".into() }, LyricLine { start_ms: 0, words: "Two".into() }]));
        assert_eq!(pick(vec![record(400.0, Some("[00:01.00] Too long"), None)], 180.0), Found::Nothing);
        assert_eq!(pick(Vec::new(), 180.0), Found::Nothing);
    }
}
