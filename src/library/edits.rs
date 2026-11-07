//! Playlist changes as Spotify's own apps send them (`playlist4` `ListChanges`), built here and
//! sent by `fetch::post_list_changes`: add songs, remove rows, rename, describe, set a picture, and
//! add or remove a playlist in the user's library list (the rootlist).

use std::collections::{HashMap, HashSet};

use librespot_protocol::playlist4_external::op::Kind;
use librespot_protocol::playlist4_external::{
    Add, ChangeInfo, Delta, Item, ItemAttributes, ListAttributeKind, ListAttributes, ListAttributesPartialState, ListChanges,
    Op, Rem, UpdateListAttributes,
};
use protobuf::{EnumOrUnknown, MessageField};

/// The base revision of a playlist that doesn't exist yet: the first change makes it.
pub const NEW_PLAYLIST_REVISION: &[u8] = b"\0\0\0\0root";
/// Songs per add request.
pub const ADD_CHUNK: usize = 100;
/// Rows per removal request (taking out rows by place).
pub const REMOVE_CHUNK: usize = 100;

/// A playlist row as a removal by place needs it: the song, and when it was added (Unix seconds,
/// 0 when unknown).
pub type Row = (String, i64);

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn item(uri: &str, stamp: Option<i64>, public: Option<bool>) -> Item {
    let mut it = Item::new();
    it.set_uri(uri.to_owned());
    if stamp.is_some() || public.is_some() {
        let mut a = ItemAttributes::new();
        if let Some(t) = stamp {
            a.set_timestamp(t);
        }
        if let Some(p) = public {
            a.set_public(p);
        }
        it.attributes = MessageField::some(a);
    }
    it
}

fn op(kind: Kind) -> Op {
    let mut o = Op::new();
    o.set_kind(kind);
    o
}

/// Songs added at the end.
pub fn add_last(uris: &[String], now: i64) -> Op {
    let mut add = Add::new();
    add.items = uris.iter().map(|u| item(u, Some(now), None)).collect();
    add.set_add_last(true);
    let mut o = op(Kind::ADD);
    o.add = MessageField::some(add);
    o
}

/// A playlist put first in the library list (`public`: shown on the profile).
pub fn add_first(uri: &str, now: i64, public: bool) -> Op {
    let mut add = Add::new();
    add.items.push(item(uri, Some(now), Some(public)));
    add.set_from_index(0);
    let mut o = op(Kind::ADD);
    o.add = MessageField::some(add);
    o
}

/// The row at `index`, which holds `uri`.
pub fn rem_one(index: usize, uri: &str) -> Op {
    let mut rem = Rem::new();
    rem.set_from_index(index as i32);
    rem.set_length(1);
    rem.items.push(item(uri, None, None));
    let mut o = op(Kind::REM);
    o.rem = MessageField::some(rem);
    o
}

/// The list's name, description and picture; what is `None` stays as it is (`clear_picture`
/// takes the picture away).
pub fn attrs(name: Option<&str>, description: Option<&str>, picture: Option<Vec<u8>>, clear_picture: bool) -> Op {
    let mut v = ListAttributes::new();
    if let Some(n) = name {
        v.set_name(n.to_owned());
    }
    if let Some(d) = description {
        v.set_description(d.to_owned());
    }
    if let Some(p) = picture {
        v.set_picture(p);
    }
    let mut s = ListAttributesPartialState::new();
    s.values = MessageField::some(v);
    if clear_picture {
        s.no_value.push(EnumOrUnknown::new(ListAttributeKind::LIST_PICTURE));
    }
    let mut u = UpdateListAttributes::new();
    u.new_attributes = MessageField::some(s);
    let mut o = op(Kind::UPDATE_LIST_ATTRIBUTES);
    o.update_list_attributes = MessageField::some(u);
    o
}

/// One delta of `ops` by `user` on top of `base` (the revision they were made against).
pub fn changes(user: &str, base: &[u8], ops: Vec<Op>, now: i64) -> ListChanges {
    let mut info = ChangeInfo::new();
    info.set_user(user.to_owned());
    info.set_timestamp(now);
    let mut d = Delta::new();
    d.ops = ops;
    d.info = MessageField::some(info);
    let mut c = ListChanges::new();
    c.set_base_revision(base.to_vec());
    c.deltas.push(d);
    c.set_want_resulting_revisions(true);
    c.set_want_sync_result(true);
    c
}

/// Every row holding one of `remove`, highest first (so the earlier rows keep their places while
/// the later ones go).
pub fn removal_ops(current: &[String], remove: &HashSet<&str>) -> Vec<Op> {
    current
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, uri)| remove.contains(uri.as_str()))
        .map(|(i, uri)| rem_one(i, uri))
        .collect()
}

/// Where the `picked` rows of a playlist read as `before` are in it `now`. Unchanged: the same
/// places. Changed: each picked row is its song's same copy (its first, second, ...), as long as
/// that song's copies are all still there, in the same order with the same dates. `None` when one
/// isn't: which copy is which can't be told then.
pub fn rows_now(before: &[Row], picked: &[usize], now: &[Row]) -> Option<Vec<usize>> {
    if picked.iter().any(|&r| r >= before.len()) {
        return None;
    }
    if before == now {
        return Some(picked.to_vec());
    }
    // Each song's copies: their places and dates.
    fn copies(list: &[Row]) -> HashMap<&str, Vec<(usize, i64)>> {
        let mut by_song: HashMap<&str, Vec<(usize, i64)>> = HashMap::new();
        for (i, (uri, added)) in list.iter().enumerate() {
            by_song.entry(uri.as_str()).or_default().push((i, *added));
        }
        by_song
    }
    let (was, is) = (copies(before), copies(now));
    picked
        .iter()
        .map(|&r| {
            let song = before[r].0.as_str();
            let (was, is) = (was.get(song)?, is.get(song)?);
            let same = was.len() == is.len() && was.iter().zip(is).all(|(a, b)| a.1 == b.1);
            let copy = was.iter().position(|&(i, _)| i == r)?;
            same.then(|| is[copy].0)
        })
        .collect::<Option<Vec<usize>>>()
        // Lowest first again: a song moved since can put its copy before rows picked above it.
        .map(|mut rows| {
            rows.sort_unstable();
            rows
        })
}

/// The next rows to take out of `left` (places, lowest first) in one change: the highest
/// `REMOVE_CHUNK`, so the places of the rest stay as they are.
pub fn next_chunk(left: &mut Vec<usize>) -> Vec<usize> {
    left.split_off(left.len().saturating_sub(REMOVE_CHUNK))
}

/// The rows at `rows` (places in `list`), highest first: each row taken out leaves the places of
/// those still to go as they were.
pub fn row_removals(list: &[Row], rows: &[usize]) -> Vec<Op> {
    let mut rows: Vec<usize> = rows.iter().copied().filter(|&r| r < list.len()).collect();
    rows.sort_unstable();
    rows.dedup();
    rows.iter().rev().map(|&i| rem_one(i, &list[i].0)).collect()
}

/// `list` without the rows at `rows`.
pub fn take_rows(list: &mut Vec<Row>, rows: &[usize]) {
    let gone: HashSet<usize> = rows.iter().copied().collect();
    let mut i = 0;
    list.retain(|_| {
        i += 1;
        !gone.contains(&(i - 1))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;

    fn roundtrip(c: &ListChanges) -> ListChanges {
        ListChanges::parse_from_bytes(&c.write_to_bytes().expect("every required field set")).unwrap()
    }

    #[test]
    fn adds_go_last_with_a_time_and_the_base_revision() {
        let uris = vec!["spotify:track:a".to_owned(), "spotify:track:b".to_owned()];
        let c = roundtrip(&changes("me", b"rev1", vec![add_last(&uris, 42)], 7));
        assert_eq!(c.base_revision(), b"rev1");
        assert!(c.want_resulting_revisions());
        let d = &c.deltas[0];
        assert_eq!((d.info.user(), d.info.timestamp()), ("me", 7));
        let o = &d.ops[0];
        assert_eq!(o.kind(), Kind::ADD);
        assert!(o.add.add_last());
        let got: Vec<(&str, i64)> = o.add.items.iter().map(|i| (i.uri(), i.attributes.timestamp())).collect();
        assert_eq!(got, [("spotify:track:a", 42), ("spotify:track:b", 42)]);
    }

    #[test]
    fn a_new_playlist_goes_first_in_the_library_list() {
        let c = roundtrip(&changes("me", NEW_PLAYLIST_REVISION, vec![add_first("spotify:playlist:x", 1, false)], 1));
        assert_eq!(c.base_revision(), NEW_PLAYLIST_REVISION);
        let add = &c.deltas[0].ops[0].add;
        assert_eq!(add.from_index(), 0);
        assert!(!add.add_last());
        assert_eq!(add.items[0].uri(), "spotify:playlist:x");
        assert!(add.items[0].attributes.has_public() && !add.items[0].attributes.public());
    }

    #[test]
    fn only_the_given_attributes_are_set() {
        let c = roundtrip(&changes("me", b"r", vec![attrs(Some("New name"), None, None, false)], 1));
        let o = &c.deltas[0].ops[0];
        assert_eq!(o.kind(), Kind::UPDATE_LIST_ATTRIBUTES);
        let v = &o.update_list_attributes.new_attributes.values;
        assert_eq!(v.name(), "New name");
        assert!(!v.has_description() && !v.has_picture());
        assert!(o.update_list_attributes.new_attributes.no_value.is_empty());
        // An empty description is sent (it clears it); a cleared picture is named.
        let c = roundtrip(&changes("me", b"r", vec![attrs(None, Some(""), None, true)], 1));
        let s = &c.deltas[0].ops[0].update_list_attributes.new_attributes;
        assert!(s.values.has_description() && s.values.description().is_empty());
        assert_eq!(s.no_value[0].enum_value(), Ok(ListAttributeKind::LIST_PICTURE));
    }

    #[test]
    fn rows_found_again_come_lowest_first() {
        let row = |u: &str, d: i64| (u.to_owned(), d);
        // x twice, then y and z; the second x and z are picked.
        let before = vec![row("x", 1), row("y", 2), row("x", 3), row("z", 4)];
        // z was moved to the top on another device: its row now comes before the second x.
        let now = vec![row("z", 4), row("x", 1), row("y", 2), row("x", 3)];
        assert_eq!(rows_now(&before, &[2, 3], &now), Some(vec![0, 3]));
        // So the highest go first and nothing below them moves before it is taken.
        let mut left = rows_now(&before, &[2, 3], &now).unwrap();
        assert_eq!(next_chunk(&mut left), vec![0, 3]);
    }

    #[test]
    fn removals_take_every_copy_from_the_end() {
        let current: Vec<String> = ["a", "b", "a", "c"].iter().map(|s| format!("spotify:track:{s}")).collect();
        let remove: HashSet<&str> = ["spotify:track:a", "spotify:track:z"].into_iter().collect();
        let ops = removal_ops(&current, &remove);
        let got: Vec<(i32, i32, &str)> = ops.iter().map(|o| (o.rem.from_index(), o.rem.length(), o.rem.items[0].uri())).collect();
        assert_eq!(got, [(2, 1, "spotify:track:a"), (0, 1, "spotify:track:a")]);
        assert!(removal_ops(&current, &HashSet::new()).is_empty());
        let c = roundtrip(&changes("me", b"r", ops, 1));
        assert!(c.deltas[0].ops.iter().all(|o| o.kind() == Kind::REM));
    }

    fn rows(list: &[(&str, i64)]) -> Vec<Row> {
        list.iter().map(|(s, added)| (format!("spotify:track:{s}"), *added)).collect()
    }

    /// `list` after `ops` as Spotify applies them, one after the other; each must name the song at
    /// its place.
    fn apply(list: &mut Vec<Row>, ops: &[Op]) {
        for o in ops {
            let at = o.rem.from_index() as usize;
            assert_eq!(o.rem.length(), 1);
            assert_eq!(list[at].0, o.rem.items[0].uri(), "row {at} isn't the song named");
            list.remove(at);
        }
    }

    #[test]
    fn rows_go_by_place_and_only_those() {
        // Three copies of a: only the second and third go, highest first.
        let before = rows(&[("a", 1), ("b", 2), ("a", 3), ("c", 4), ("a", 5)]);
        let ops = row_removals(&before, &[2, 4]);
        let got: Vec<i32> = ops.iter().map(|o| o.rem.from_index()).collect();
        assert_eq!(got, [4, 2]);
        let mut list = before.clone();
        apply(&mut list, &ops);
        assert_eq!(list, rows(&[("a", 1), ("b", 2), ("c", 4)]));
        let mut taken = before.clone();
        take_rows(&mut taken, &[4, 2]);
        assert_eq!(taken, list);
        // A row named twice goes once; one that isn't there, not at all.
        assert_eq!(row_removals(&before, &[2, 2, 9]).len(), 1);
        assert!(row_removals(&before, &[]).is_empty());
    }

    #[test]
    fn many_rows_go_in_chunks_from_the_end() {
        let total = REMOVE_CHUNK * 2 + REMOVE_CHUNK / 2;
        let mut list: Vec<Row> = (0..total).map(|i| (format!("spotify:track:{i}"), i as i64)).collect();
        let mut left: Vec<usize> = (0..total).filter(|i| i % 2 == 1).collect();
        let (mut sizes, mut below) = (Vec::new(), usize::MAX);
        while !left.is_empty() {
            let chunk = next_chunk(&mut left);
            // Every change is below the ones before it, so its places are still right.
            assert!(chunk.iter().all(|&r| r < below));
            below = chunk[0];
            sizes.push(chunk.len());
            let ops = row_removals(&list, &chunk);
            apply(&mut list, &ops);
        }
        assert_eq!(sizes, [REMOVE_CHUNK, total / 2 - REMOVE_CHUNK]);
        let kept: Vec<Row> = (0..total).filter(|i| i % 2 == 0).map(|i| (format!("spotify:track:{i}"), i as i64)).collect();
        assert_eq!(list, kept);
    }

    #[test]
    fn picked_rows_are_found_again_after_a_change() {
        let before = rows(&[("a", 1), ("b", 2), ("a", 3), ("c", 4)]);
        assert_eq!(rows_now(&before, &[2, 3], &before), Some(vec![2, 3]));
        // Songs added above: each row is its song's same copy.
        let now = rows(&[("x", 9), ("y", 8), ("a", 1), ("b", 2), ("a", 3), ("c", 4)]);
        assert_eq!(rows_now(&before, &[2, 3], &now), Some(vec![4, 5]));
        // Other songs moved or gone: no matter.
        let now = rows(&[("a", 1), ("a", 3), ("c", 4)]);
        assert_eq!(rows_now(&before, &[2], &now), Some(vec![1]));
        // The picked song's copies moved round, or one is gone or added: which is which can't be told.
        assert_eq!(rows_now(&before, &[2], &rows(&[("a", 3), ("b", 2), ("a", 1), ("c", 4)])), None);
        assert_eq!(rows_now(&before, &[2], &rows(&[("a", 1), ("b", 2), ("c", 4)])), None);
        assert_eq!(rows_now(&before, &[2], &rows(&[("a", 1), ("b", 2), ("a", 3), ("c", 4), ("a", 6)])), None);
        // Dates not known: the same number of copies will do.
        let before = rows(&[("a", 0), ("b", 0), ("a", 0)]);
        assert_eq!(rows_now(&before, &[2], &rows(&[("z", 0), ("a", 0), ("b", 0), ("a", 0)])), Some(vec![3]));
        // A place that isn't in the playlist read.
        assert_eq!(rows_now(&before, &[7], &before), None);
    }
}
