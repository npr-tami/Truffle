//! Small formatting and platform helpers shared by the UI modules.

use egui::Color32;

use super::accent::contrast_ratio;

/// `m:ss`, or `h:mm:ss` for long tracks.
pub fn fmt_ms(ms: u32) -> String {
    let total = ms / 1000;
    let (h, m, s) = (total / 3600, (total / 60) % 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// "3 h 12 min", "12 min", "45 s".
pub fn fmt_total(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m) = (secs / 3600, (secs / 60) % 60);
    if h > 0 {
        format!("{h} h {m} min")
    } else if m > 0 {
        format!("{m} min")
    } else {
        format!("{secs} s")
    }
}

/// "Oct 2" (this year) or "Oct 2, 2024", from Unix seconds; empty when unknown.
pub fn fmt_added(secs: i64, now_secs: i64) -> String {
    if secs <= 0 {
        return String::new();
    }
    // Days since 1970 to a calendar date (Howard Hinnant's civil_from_days).
    let date = |secs: i64| {
        let z = secs.div_euclid(86_400) + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (yoe + era * 400 + i64::from(m <= 2), m, d)
    };
    let (y, m, d) = date(secs);
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let month = MONTHS[(m - 1).clamp(0, 11) as usize];
    if y == date(now_secs).0 { format!("{month} {d}") } else { format!("{month} {d}, {y}") }
}

/// "1 song" / "1,234 songs".
pub fn fmt_song_count(n: usize) -> String {
    if n == 1 { "1 song".to_owned() } else { format!("{} songs", fmt_thousands(n)) }
}

pub fn fmt_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// `spotify:track:ID` -> `https://open.spotify.com/track/ID` (also playlists, albums, artists,
/// episodes and the old `spotify:user:NAME:playlist:ID` form). Local files have no link.
pub fn spotify_web_link(uri: &str) -> Option<String> {
    let parts: Vec<&str> = uri.split(':').collect();
    match parts.as_slice() {
        ["spotify", "user", _, "playlist", id] => Some(format!("https://open.spotify.com/playlist/{id}")),
        ["spotify", kind @ ("track" | "album" | "artist" | "playlist" | "episode" | "show" | "user"), id]
            if !id.is_empty() =>
        {
            Some(format!("https://open.spotify.com/{kind}/{id}"))
        }
        _ => None,
    }
}

/// Opens a web page in the user's default browser. eframe is built without its `links` feature,
/// so `ctx.open_url` would do nothing on its own; Windows' shell handles it instead.
pub fn open_in_browser(ctx: &egui::Context, url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        log::warn!("refusing to open non-web link {url}");
        return;
    }
    #[cfg(windows)]
    {
        match std::process::Command::new("explorer").arg(url).spawn() {
            Ok(_) => return,
            Err(e) => log::warn!("could not open the browser for {url}: {e}"),
        }
    }
    ctx.open_url(egui::OpenUrl::new_tab(url));
}

/// Playlist descriptions arrive as light HTML (links, `&amp;` entities). Make them plain text.
pub fn clean_description(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len());
    let mut in_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    decode_entities(&text).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(end) = rest.char_indices().take(12).find(|&(_, c)| c == ';').map(|(i, _)| i) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse::<u32>().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Black or white, whichever reads better on `bg` (at least 4.5 to 1 on any colour).
pub fn contrast_text(bg: Color32) -> Color32 {
    if contrast_ratio(Color32::BLACK, bg) > contrast_ratio(Color32::WHITE, bg) { Color32::BLACK } else { Color32::WHITE }
}

/// Linear blend between two colours (alpha ignored).
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(lerp(a.r(), b.r()), lerp(a.g(), b.g()), lerp(a.b(), b.b()))
}

/// A Spotify client id is 32 hexadecimal characters.
pub fn looks_like_client_id(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// A Discord Application ID is a long decimal number (a "snowflake", about 18 digits).
pub fn looks_like_discord_id(s: &str) -> bool {
    (16..=21).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_ms(0), "0:00");
        assert_eq!(fmt_ms(61_000), "1:01");
        assert_eq!(fmt_ms(3_723_000), "1:02:03");
        assert_eq!(fmt_total(3_723_000), "1 h 2 min");
        assert_eq!(fmt_total(125_000), "2 min");
        assert_eq!(fmt_thousands(1_234_567), "1,234,567");
        assert_eq!(fmt_song_count(1), "1 song");
    }

    #[test]
    fn added_dates_read_like_spotify() {
        // 2026-10-02 and 2024-03-15, seen on 2026-10-25.
        let now = 1_792_886_400;
        assert_eq!(fmt_added(1_790_899_200, now), "Oct 2");
        assert_eq!(fmt_added(1_710_460_800, now), "Mar 15, 2024");
        assert_eq!(fmt_added(0, now), "");
    }

    #[test]
    fn builds_links() {
        assert_eq!(spotify_web_link("spotify:track:abc").as_deref(), Some("https://open.spotify.com/track/abc"));
        assert_eq!(
            spotify_web_link("spotify:user:bob:playlist:xyz").as_deref(),
            Some("https://open.spotify.com/playlist/xyz")
        );
        assert_eq!(spotify_web_link("spotify:local:a:b:c:12"), None);
    }

    #[test]
    fn recognises_discord_ids() {
        assert!(looks_like_discord_id("1234567890123456789"));
        assert!(!looks_like_discord_id("12345"));
        assert!(!looks_like_discord_id("12345678901234567x9"));
        assert!(!looks_like_discord_id(""));
    }

    #[test]
    fn cleans_descriptions() {
        assert_eq!(
            clean_description("Best of <a href=\"x\">rock</a> &amp; roll &#x27;80s&#39;"),
            "Best of rock & roll '80s'"
        );
        assert_eq!(clean_description("a & b"), "a & b");
    }

    #[test]
    fn words_on_any_colour_read() {
        // Every grey and the mid tones where black and white read about the same.
        let greys = (0..=255).map(Color32::from_gray);
        let mids = [[138, 92, 246], [236, 72, 153], [239, 68, 68], [227, 38, 45], [30, 144, 255], [128, 128, 128]].map(|[r, g, b]| Color32::from_rgb(r, g, b));
        for bg in greys.chain(mids) {
            assert!(contrast_ratio(contrast_text(bg), bg) >= 4.5, "{bg:?}");
        }
    }
}
