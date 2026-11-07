//! Text folded for filters: lower case without accents, so "motley" finds "Mötley Crüe" and
//! "mötley" finds "Motley". Both sides are folded, what is typed and what is searched. Latin
//! letters lose their marks (ø is o, æ is ae, ß is ss), full-width ＡＢＣ is plain ABC and curly
//! quotes are straight. Letters styled the way "fancy font" sites style them are plain ones (𝓮, ⓔ,
//! 🅔, ᴇ and ᵉ are e), and marks piled on letters (zalgo) go. Other scripts (Cyrillic, Greek,
//! Japanese) are only lower-cased; people's names also lose the Cyrillic and Greek letters that
//! stand in for Latin ones ([`fold_name`]).

use std::ops::Range;

/// `s` folded: "Mötley Crüe" is "motley crue", "Straße" is "strasse", "𝓔𝓶𝓶𝓪" is "emma".
pub fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    push_fold(&mut out, s);
    out
}

/// `s` folded as people's names are matched: as [`fold`], and the Cyrillic and Greek letters that
/// look like Latin ones are those ("Еmmа" with a Cyrillic Е and а is "emma"). Only for names:
/// song filters keep Cyrillic and Greek as they are.
pub fn fold_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    push(&mut out, s, true);
    out
}

/// Adds `s`, folded, to the end of `out`.
pub fn push_fold(out: &mut String, s: &str) {
    push(out, s, false);
}

fn push(out: &mut String, s: &str, names: bool) {
    if s.is_ascii() {
        let start = out.len();
        out.push_str(s);
        out[start..].make_ascii_lowercase();
    } else {
        for c in s.chars() {
            push_char(out, c, names);
        }
    }
}

/// Whether `hay`, folded, holds `needle` (folded already). Plain ASCII is compared in place,
/// without a folded copy.
pub fn contains_folded(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.is_ascii() {
        let n = needle.as_bytes();
        return hay.as_bytes().windows(n.len()).any(|w| w.eq_ignore_ascii_case(n));
    }
    fold(hay).contains(needle)
}

/// Where `needle` (folded already) first shows in `hay`, as a range of `hay` that never cuts a
/// character: "crue" in "Mötley Crüe" is 8..13. A match on part of a letter that folds to two
/// ("a" of "æ") takes the whole letter, and marks after the last letter go with it.
pub fn find_folded(hay: &str, needle: &str) -> Option<Range<usize>> {
    find(hay, needle, false)
}

/// [`find_folded`] in a person's name, `needle` folded by [`fold_name`].
pub fn find_folded_name(hay: &str, needle: &str) -> Option<Range<usize>> {
    find(hay, needle, true)
}

fn find(hay: &str, needle: &str, names: bool) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    if hay.is_ascii() {
        let n = needle.as_bytes();
        let at = hay.as_bytes().windows(n.len()).position(|w| w.eq_ignore_ascii_case(n))?;
        return Some(at..at + n.len());
    }
    // The folded text, and for each of its bytes where its character starts in `hay`.
    let mut folded = String::with_capacity(hay.len());
    let mut from = Vec::with_capacity(hay.len());
    for (i, c) in hay.char_indices() {
        push_char(&mut folded, c, names);
        from.resize(folded.len(), i);
    }
    let at = folded.find(needle)?;
    let last = from[at + needle.len() - 1];
    let end = match from.get(at + needle.len()) {
        Some(&next) if next > last => next,
        // It ends inside a letter that folds to two: the whole letter.
        Some(_) => last + hay[last..].chars().next().map_or(0, char::len_utf8),
        None => hay.len(),
    };
    Some(from[at]..end)
}

fn push_char(out: &mut String, c: char, names: bool) {
    // A styled letter is the one it stands for (𝓔 is E, ⓔ is e).
    let c = if c.is_ascii() { c } else { styled(c).unwrap_or(c) };
    if c.is_ascii() {
        out.push(c.to_ascii_lowercase());
        return;
    }
    if names && let Some(l) = looks_latin(c) {
        out.push(l);
        return;
    }
    // Lower case first, so both cases of a letter fold alike ("İ" is "i" and a dot above).
    for c in c.to_lowercase() {
        match c {
            // Full-width ！ to ～ (Japanese and Chinese keyboards): the ASCII ones.
            '\u{ff01}'..='\u{ff5e}' => out.push(char::from_u32(c as u32 - 0xfee0).unwrap_or(c)),
            _ => match plain(c) {
                Some(p) => out.push_str(p),
                None => out.push(c),
            },
        }
    }
}

/// The plain letter or digit a styled one stands for, as "fancy font" sites use them: the maths
/// letters and digits (𝐀 𝓮 𝔢 𝕖 𝟙, Greek too) with the letterlike symbols in their gaps (ℯ ℂ),
/// circled, bracketed and squared letters (ⓔ ⒠ 🄴 🅔 🅴), regional indicators (🇪), small
/// capitals (ᴇ), and raised and lowered letters (ᵉ ₑ).
fn styled(c: char) -> Option<char> {
    let at = c as u32;
    // The `i`th letter, the 26 capitals and the 26 small ones alike.
    let letter = |i: u32| char::from_u32('a' as u32 + i % 26);
    match at {
        // Maths: 13 styles of A to Z then a to z, then a dotless i and j.
        0x1d400..=0x1d6a3 => letter(at - 0x1d400),
        0x1d6a4 => Some('i'),
        0x1d6a5 => Some('j'),
        // 5 styles of Greek, 58 each, and a digamma.
        0x1d6a8..=0x1d7c9 => GREEK.chars().nth(((at - 0x1d6a8) % 58) as usize),
        0x1d7ca => Some('\u{3dc}'),
        0x1d7cb => Some('\u{3dd}'),
        // 5 styles of 0 to 9.
        0x1d7ce..=0x1d7ff => char::from_u32('0' as u32 + (at - 0x1d7ce) % 10),
        // Circled Ⓐ to ⓩ, bracketed ⒜ to ⒵.
        0x24b6..=0x24e9 => letter(at - 0x24b6),
        0x249c..=0x24b5 => letter(at - 0x249c),
        // Bracketed 🄐, squared 🄰, negative circled 🅐 and negative squared 🅰 capitals: A to Z at
        // the start of each 32 (signs such as 🅊 fill the rest).
        0x1f110..=0x1f189 if (at - 0x1f110) % 32 < 26 => letter((at - 0x1f110) % 32),
        // Regional indicators 🇦 to 🇿 (two of them make a flag).
        0x1f1e6..=0x1f1ff => letter(at - 0x1f1e6),
        _ => Some(match c {
            // Small capitals (and Ɪ and Ʀ, the capitals of two of them), raised and lowered
            // letters, and the letterlike symbols.
            'ᴀ' | 'ᵃ' | 'ᴬ' | 'ₐ' => 'a',
            'ʙ' | 'ᵇ' | 'ᴮ' | 'ℬ' => 'b',
            'ᴄ' | 'ᶜ' | 'ℭ' | 'ℂ' => 'c',
            'ᴅ' | 'ᵈ' | 'ᴰ' | 'ⅅ' | 'ⅆ' => 'd',
            'ᴇ' | 'ᵉ' | 'ᴱ' | 'ₑ' | 'ℯ' | 'ℰ' | 'ⅇ' => 'e',
            'ꜰ' | 'ᶠ' | 'ℱ' => 'f',
            'ɢ' | 'ᵍ' | 'ᴳ' | 'ℊ' => 'g',
            'ʜ' | 'ʰ' | 'ᴴ' | 'ₕ' | 'ℎ' | 'ℋ' | 'ℌ' | 'ℍ' => 'h',
            'ɪ' | '\u{a7ae}' | 'ⁱ' | 'ᴵ' | 'ᵢ' | 'ℐ' | 'ℑ' | 'ⅈ' => 'i',
            'ᴊ' | 'ʲ' | 'ᴶ' | 'ⱼ' | 'ⅉ' => 'j',
            'ᴋ' | 'ᵏ' | 'ᴷ' | 'ₖ' => 'k',
            'ʟ' | 'ˡ' | 'ᴸ' | 'ₗ' | 'ℒ' => 'l',
            'ᴍ' | 'ᵐ' | 'ᴹ' | 'ₘ' | 'ℳ' => 'm',
            'ɴ' | 'ⁿ' | 'ᴺ' | 'ₙ' | 'ℕ' => 'n',
            'ᴏ' | 'ᵒ' | 'ᴼ' | 'ₒ' | 'ℴ' => 'o',
            'ᴘ' | 'ᵖ' | 'ᴾ' | 'ₚ' | 'ℙ' => 'p',
            'ℚ' | '\u{107a5}' => 'q',
            'ʀ' | '\u{1a6}' | 'ʳ' | 'ᴿ' | 'ᵣ' | 'ℛ' | 'ℜ' | 'ℝ' => 'r',
            'ꜱ' | 'ˢ' | 'ₛ' => 's',
            'ᴛ' | 'ᵗ' | 'ᵀ' | 'ₜ' => 't',
            'ᴜ' | 'ᵘ' | 'ᵁ' | 'ᵤ' => 'u',
            'ᴠ' | 'ᵛ' | 'ⱽ' | 'ᵥ' => 'v',
            'ᴡ' | 'ʷ' | 'ᵂ' => 'w',
            'ˣ' | 'ₓ' => 'x',
            'ʏ' | 'ʸ' => 'y',
            'ᴢ' | 'ᶻ' | 'ℤ' | 'ℨ' => 'z',
            _ => return None,
        }),
    }
}

/// The Latin letter a Cyrillic or Greek one looks like, where fancy names use it for one: а е о р
/// с у х і ј ѕ and ο α ε ι κ ν τ υ ρ, and their capitals (Greek Ν and Υ look like N and Y, small ν
/// and υ like v and u). And ǫ, the small capital q of fancy fonts (in other text an o with a hook).
fn looks_latin(c: char) -> Option<char> {
    Some(match c {
        '\u{430}' | '\u{410}' | '\u{3b1}' | '\u{391}' => 'a',
        '\u{441}' | '\u{421}' => 'c',
        '\u{435}' | '\u{415}' | '\u{3b5}' | '\u{395}' => 'e',
        '\u{456}' | '\u{406}' | '\u{3b9}' | '\u{399}' => 'i',
        '\u{458}' | '\u{408}' => 'j',
        '\u{3ba}' | '\u{39a}' => 'k',
        '\u{39d}' => 'n',
        '\u{43e}' | '\u{41e}' | '\u{3bf}' | '\u{39f}' => 'o',
        '\u{440}' | '\u{420}' | '\u{3c1}' | '\u{3a1}' => 'p',
        'ǫ' => 'q',
        '\u{455}' | '\u{405}' => 's',
        '\u{3c4}' | '\u{3a4}' => 't',
        '\u{3c5}' => 'u',
        '\u{3bd}' => 'v',
        '\u{445}' | '\u{425}' => 'x',
        '\u{443}' | '\u{423}' | '\u{3a5}' => 'y',
        _ => return None,
    })
}

/// What a lower-case character folds to: its plain letters, "" for a mark that sits on the letter
/// before it ("o" and U+0308 is "ö" taken apart), or `None` to keep it.
fn plain(c: char) -> Option<&'static str> {
    let at = c as usize;
    let (table, i) = match c {
        // Marks that sit on letters (zalgo piles them up; Japanese ones, the dots of が, stay),
        // and the switch to an emoji's look or a letter's (🅰️).
        '\u{300}'..='\u{36f}'
        | '\u{1ab0}'..='\u{1aff}'
        | '\u{1dc0}'..='\u{1dff}'
        | '\u{20d0}'..='\u{20ff}'
        | '\u{fe20}'..='\u{fe2f}'
        | '\u{fe0e}'
        | '\u{fe0f}' => return Some(""),
        'æ' | 'ǣ' | 'ǽ' => return Some("ae"),
        'œ' => return Some("oe"),
        'ß' => return Some("ss"),
        'þ' => return Some("th"),
        'ĳ' => return Some("ij"),
        'ǆ' | 'ǳ' => return Some("dz"),
        'ǉ' => return Some("lj"),
        'ǌ' => return Some("nj"),
        // Greek: a final sigma is a sigma.
        'ς' => return Some("σ"),
        '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' | '\u{2bc}' => return Some("'"),
        '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => return Some("\""),
        '\u{c0}'..='\u{ff}' => (LATIN_1, at - 0xc0),
        '\u{100}'..='\u{17f}' => (EXTENDED_A, at - 0x100),
        '\u{180}'..='\u{24f}' => (EXTENDED_B, at - 0x180),
        '\u{1e00}'..='\u{1e95}' => (ADDITIONAL, (at - 0x1e00) / 2),
        '\u{1e96}'..='\u{1e9d}' => (ADDITIONAL_LOWER, at - 0x1e96),
        '\u{1ea0}'..='\u{1ef9}' => (VIETNAMESE, (at - 0x1ea0) / 2),
        _ => return None,
    };
    table.get(i..=i).filter(|&p| p != ".")
}

// The plain letter of each character of a block, both cases; "." keeps the character (× and ÷,
// letters of their own such as ǝ, and the ones above that fold to two letters).

/// U+00C0 to U+00FF: À to ÿ.
const LATIN_1: &str = "aaaaaa.ceeeeiiiidnooooo.ouuuuy..aaaaaa.ceeeeiiiidnooooo.ouuuuy.y";
/// U+0100 to U+017F: Ā to ſ.
const EXTENDED_A: &str = "aaaaaaccccccccddddeeeeeeeeeegggggggghhhhiiiiiiiiii..jjkkkllllllllllnnnnnnnnn\
    oooooo..rrrrrrssssssssttttttuuuuuuuuuuuuwwyyyzzzzzzs";
/// U+0180 to U+024F: ƀ to ɏ.
const EXTENDED_B: &str = "bbbb...ccdddd....ffg...ikkl..nnooo..pp.....ttttuu.vyyzz......................aai\
    ioouuuuuuuuuu.aaaa..ggggkkoooo..j...gg..nnaa..ooaaaaeeeeiiiioooorrrruuuusstt..hhnd..zzaaeeoooooo\
    ooyylntj..accltsz..bu.eejjqqrryy";
/// U+1E00 to U+1E95 in pairs (capital, small): Ḁḁ to Ẕẕ.
const ADDITIONAL: &str = "abbbcdddddeeeeefghhhhhiikkkllllmmmnnnnoooopprrrrsssssttttuuuuuvvwwwwwxxyzzz";
/// U+1E96 to U+1E9D: ẖ to ẝ, small letters only.
const ADDITIONAL_LOWER: &str = "htwyasss";
/// U+1EA0 to U+1EF9 in pairs, Vietnamese: Ạạ to Ỹỹ.
const VIETNAMESE: &str = "aaaaaaaaaaaaeeeeeeeeiioooooooooooouuuuuuuyyyy";
/// The 58 Greek letters and signs of each maths style, from U+1D6A8: Α to Ω, ∇, α to ω, ∂ and the
/// other shapes of ε θ κ φ ρ π.
const GREEK: &str = "ΑΒΓΔΕΖΗΘΙΚΛΜΝΞΟΠΡϴΣΤΥΦΧΨΩ∇αβγδεζηθικλμνξοπρςστυφχψω∂ϵϑϰϕϱϖ";

#[cfg(test)]
mod tests {
    use super::*;

    /// What a filter box does: the typed text folded once, then looked for.
    fn finds(hay: &str, typed: &str) -> bool {
        contains_folded(hay, &fold(typed))
    }

    #[test]
    fn accents_do_not_matter_either_way() {
        assert!(finds("Mötley Crüe", "motley"));
        assert!(finds("Mötley Crüe", "motley crue"));
        assert!(finds("Motley", "mötley"));
        assert!(finds("Beyoncé", "beyonce"));
        assert!(finds("Sigur Rós", "sigur ros"));
        assert!(finds("Björk", "bjork"));
        assert!(finds("BJÖRK", "Björk"));
        assert!(!finds("Björk", "bjark"));
    }

    #[test]
    fn letters_of_their_own_fold_to_plain_ones() {
        assert!(finds("Encyclopædia", "ae"));
        assert!(finds("Straße", "strasse"));
        assert!(finds("STRASSE", "straße"));
        assert_eq!(fold("Ørsted Łódź Đorđe Þór ǅemal İstanbul ıi Œuvre ẞ"), "orsted lodz dorde thor dzemal istanbul ii oeuvre ss");
        assert_eq!(fold("Dvořák Ștefan Țara Ångström Ñandú"), "dvorak stefan tara angstrom nandu");
        assert_eq!(fold("Tiếng Việt, Nguyễn, Đặng"), "tieng viet, nguyen, dang");
        assert_eq!(fold("Ḱḱ Ẕ ẖ ẞ"), "kk z h ss");
    }

    #[test]
    fn full_width_and_curly_quotes_are_plain() {
        assert_eq!(fold("ＢＴＳ！ｂｔｓ"), "bts!bts");
        assert!(finds("Don’t Stop", "don't"));
        assert!(finds("Don't Stop", "Don’t"));
        assert_eq!(fold("“Hi” ‘there’"), "\"hi\" 'there'");
    }

    #[test]
    fn other_scripts_are_only_lower_cased() {
        assert!(finds("Кино", "КИНО"));
        assert!(finds("ГРУППА КРОВИ", "группа"));
        assert_eq!(fold("Ёлка"), "ёлка");
        assert!(finds("ΣΑΣ", "σας"));
        assert!(finds("宇多田ヒカル", "ヒカル"));
        assert_eq!(fold("방탄소년단"), "방탄소년단");
        // Letters that look Latin stay Cyrillic and Greek in song filters (only names fold them).
        assert_eq!(fold("Сосна ΤΥΡΟ"), "сосна τυρο");
        assert!(!finds("Сосна", "cocha") && !finds("Emma", "\u{415}mma"));
    }

    #[test]
    fn styled_letters_are_plain_ones_either_way() {
        // Each maths style of "Emma" (ℰ fills a gap of the script one).
        let maths = ["𝐄𝐦𝐦𝐚", "𝐸𝑚𝑚𝑎", "𝑬𝒎𝒎𝒂", "ℰ𝓂𝓂𝒶", "𝓔𝓶𝓶𝓪", "𝔈𝔪𝔪𝔞", "𝔼𝕞𝕞𝕒", "𝕰𝖒𝖒𝖆", "𝖤𝗆𝗆𝖺", "𝗘𝗺𝗺𝗮", "𝘌𝘮𝘮𝘢", "𝙀𝙢𝙢𝙖", "𝙴𝚖𝚖𝚊"];
        // Circled, bracketed, squared, negative (one with its emoji look), flag letters, small
        // capitals, raised and lowered.
        let others = ["Ⓔⓜⓜⓐ", "⒠⒨⒨⒜", "🄔🄜🄜🄐", "🄴🄼🄼🄰", "🅔🅜🅜🅐", "🅴🅼🅼🅰", "🅴\u{fe0f}🅼🅼🅰\u{fe0f}", "🇪🇲🇲🇦", "ᴇᴍᴍᴀ", "ᴱᵐᵐᵃ", "ₑₘₘₐ"];
        for fancy in maths.into_iter().chain(others) {
            assert_eq!(fold(fancy), "emma", "{fancy}");
            assert!(finds(&format!("I ♥ {fancy}!"), "emma"), "plain letters find {fancy}");
            assert!(finds("Emma Stone", fancy) && finds(fancy, "EMMA"), "{fancy} finds plain letters");
        }
        assert!(finds("𝓢𝓲𝓰𝓾𝓻 Rós", "sigur ros"));
        // The letterlike symbols in the maths gaps, and digits.
        assert_eq!(fold("ℎℯℊℴ ℬℰℱℋℐℒℳℛ ℭℌℑℜℨ ℂℍℕℙℚℝℤ ⅅⅆⅇⅈⅉ"), "hego befhilmr chirz chnpqrz ddeij");
        assert_eq!(fold("𝟏𝟗𝟖𝟒 𝟙𝟡𝟠𝟜 𝟷𝟿𝟾𝟺"), "1984 1984 1984");
        // Small capitals A to Z (there is no x: fancy fonts use x itself).
        assert_eq!(fold("ᴀʙᴄᴅᴇꜰɢʜɪᴊᴋʟᴍɴᴏᴘʀꜱᴛᴜᴠᴡxʏᴢ"), "abcdefghijklmnoprstuvwxyz");
        assert_eq!(fold("\u{a7ae}\u{1a6}"), fold("ɪʀ"));
        // Maths Greek is Greek.
        assert_eq!(fold("𝛂𝛃𝛄 𝚺𝛓"), "αβγ σσ");
    }

    #[test]
    fn piled_up_marks_go_but_japanese_ones_stay() {
        // Zalgo, with marks from each block of them.
        let zalgo = "E\u{338}\u{322}\u{34e}\u{343}m\u{337}\u{1ab5}m\u{1dc4}\u{20d7}a\u{fe21}\u{336}";
        assert_eq!(fold(zalgo), "emma");
        assert!(finds(zalgo, "emma") && finds("Emma", zalgo));
        assert_eq!(find_folded(&format!("{zalgo}!"), "emma"), Some(0..zalgo.len()));
        assert_eq!(fold("\u{301}\u{1ab0}\u{20d0}"), "");
        // Japanese voicing marks belong to their letter (か and ゙ is が).
        assert_eq!(fold("か\u{3099}"), "か\u{3099}");
        assert!(!finds("か", "か\u{3099}"));
    }

    #[test]
    fn emoji_and_right_to_left_text_keep_their_shape() {
        assert_eq!(fold("🔥🎵💜"), "🔥🎵💜");
        // An emoji's switch to its emoji look goes: ❤️ and ❤ match.
        assert!(finds("❤️ Love", "❤ love") && finds("❤ Love", "❤️"));
        assert_eq!(fold("مرحبا שָׁלוֹם"), "مرحبا שָׁלוֹם");
        assert!(finds("\u{202e}Emma", "emma"));
        assert_eq!(find_folded("שלום Emma", "emma"), Some(9..13));
    }

    #[test]
    fn names_also_fold_letters_that_look_latin() {
        // A Cyrillic Е and а in a Latin name; Greek capitals; small Greek as it looks (ν is v).
        assert_eq!(fold_name("\u{415}mm\u{430}"), "emma");
        assert_eq!(fold_name("ΚΑΤΕ ΝΙΚΟ ΤΥΡΟ"), "kate niko typo");
        assert_eq!(fold_name("νικο"), "viko");
        // After the styles: maths Greek capitals look Latin too.
        assert_eq!(fold_name("𝚱𝚨𝚻𝚬 𝛮𝛶"), "kate ny");
        // ǫ is the small capital q in a name, an o with a hook in other text.
        assert_eq!(fold_name("ǫᴜᴇᴇɴ"), "queen");
        assert_eq!(fold("ǫᴜᴇᴇɴ"), "oueen");
        // Letters that look like no Latin one stay (к, и and н of Кино), and the rest is as `fold`
        // has it.
        assert_eq!(fold_name("Кино"), format!("{}o", fold("Кин")));
        for s in ["Emma Stone", "Mötley Crüe", "𝓔𝓶𝓶𝓪 🔥", "宇多田ヒカル", "", "  "] {
            assert_eq!(fold_name(s), fold(s), "{s}");
        }
        assert_eq!(find_folded_name("Hi \u{415}mm\u{430}!", "emma"), Some(3..9));
        assert_eq!(find_folded("Hi \u{415}mm\u{430}!", "emma"), None);
    }

    #[test]
    fn very_long_styled_text_folds_whole() {
        let long = "𝓔𝓶𝓶𝓪 ".repeat(5000);
        assert_eq!(fold(&long), "emma ".repeat(5000));
        assert_eq!(fold_name(&long), "emma ".repeat(5000));
        let end = format!("{long}ℤ𝔞𝔩𝔤𝔬");
        assert_eq!(find_folded(&end, "zalgo"), Some(long.len()..end.len()));
    }

    #[test]
    fn taken_apart_accents_fold_too() {
        assert_eq!(fold("Mo\u{308}tley Cru\u{308}e"), "motley crue");
        assert!(finds("Mötley", "mo\u{308}tley"));
        assert!(finds("Mo\u{308}tley", "mötley"));
    }

    #[test]
    fn nothing_typed_finds_everything() {
        assert!(contains_folded("Anything", ""));
        assert!(contains_folded("Mötley", ""));
        assert!(contains_folded("", ""));
        assert!(!contains_folded("", "a"));
        assert_eq!(find_folded("Mötley", ""), None);
    }

    #[test]
    fn matches_are_ranges_of_the_shown_text() {
        assert_eq!(find_folded("Mötley Crüe", "crue"), Some(8..13));
        assert_eq!(find_folded("Mötley Crüe", "mot"), Some(0..4));
        assert_eq!(find_folded("Neon Rain", "rain"), Some(5..9));
        // A two-letter fold matched in part takes the whole letter.
        assert_eq!(find_folded("Æther", "e"), Some(0..2));
        assert_eq!(find_folded("Straße", "as"), Some(3..6));
        // Marks after the last letter go with it.
        assert_eq!(find_folded("Mo\u{308}t", "mo"), Some(0..4));
        assert_eq!(find_folded("Cru\u{308}", "cru"), Some(0..5));
        assert_eq!(find_folded("Björk", "x"), None);
    }

    #[test]
    fn mixed_text_never_cuts_a_character() {
        let hays = [
            "Mötley Crüe – Dr. Feelgood (Remastered)",
            "Æ ß ǅ İ ﬀ ŉ Þ ẞ",
            "Ｂjörk 東京 Кино ΣΑΣ 🎵 Ðorđe",
            "a\u{308}\u{301}b\u{36f} \u{300}",
            "Đặng Thái Sơn, Nguyễn",
            "\u{301}x\u{300}",
            "𝓔𝓶𝓶𝓪 Ⓔ🅴\u{fe0f}🇬🇧 ᴇᴍᴍᴀ ᵉ e\u{336}\u{1ab5} \u{415}mm\u{430} ΚΑΤΕ ǫ",
            "\u{202e}שָׁלוֹם 🔥 か\u{3099}",
            "",
        ];
        for hay in hays {
            let folded = fold(hay);
            let ends: Vec<usize> = (0..=folded.len()).filter(|&i| folded.is_char_boundary(i)).collect();
            for (k, &a) in ends.iter().enumerate() {
                for &b in &ends[k + 1..] {
                    let needle = &folded[a..b];
                    let r = find_folded(hay, needle).unwrap_or_else(|| panic!("{needle:?} not in {hay:?}"));
                    assert!(hay.is_char_boundary(r.start) && hay.is_char_boundary(r.end) && r.start < r.end, "{needle:?} in {hay:?}: {r:?}");
                    assert!(fold(&hay[r.clone()]).contains(needle), "{needle:?} in {hay:?}: {r:?}");
                    assert!(contains_folded(hay, needle));
                }
            }
            assert_eq!(find_folded(hay, "zzzz"), None);
            // The same for names.
            let folded = fold_name(hay);
            let ends: Vec<usize> = (0..=folded.len()).filter(|&i| folded.is_char_boundary(i)).collect();
            for (k, &a) in ends.iter().enumerate() {
                for &b in &ends[k + 1..] {
                    let needle = &folded[a..b];
                    let r = find_folded_name(hay, needle).unwrap_or_else(|| panic!("{needle:?} not in {hay:?}"));
                    assert!(hay.is_char_boundary(r.start) && hay.is_char_boundary(r.end) && r.start < r.end, "{needle:?} in {hay:?}: {r:?}");
                    assert!(fold_name(&hay[r.clone()]).contains(needle), "{needle:?} in {hay:?}: {r:?}");
                }
            }
        }
    }

    #[test]
    fn tables_cover_their_blocks() {
        assert_eq!(LATIN_1.len(), 0x100 - 0xc0);
        assert_eq!(EXTENDED_A.len(), 0x180 - 0x100);
        assert_eq!(EXTENDED_B.len(), 0x250 - 0x180);
        assert_eq!(ADDITIONAL.len(), (0x1e96 - 0x1e00) / 2);
        assert_eq!(ADDITIONAL_LOWER.len(), 0x1e9e - 0x1e96);
        assert_eq!(VIETNAMESE.len(), (0x1efa - 0x1ea0) / 2);
        // Every letter folds to lower-case ASCII or stays itself.
        for c in ('\u{c0}'..='\u{24f}').chain('\u{1e00}'..='\u{1ef9}') {
            let f = fold(&c.to_string());
            assert!(f.bytes().all(|b| b.is_ascii_lowercase()) || f == c.to_lowercase().to_string(), "{c} folds to {f}");
        }
        // Every maths letter and digit is one plain one: a to z, 0 to 9, or Greek.
        assert_eq!(GREEK.chars().count(), 58);
        for c in '\u{1d400}'..='\u{1d7ff}' {
            let f = fold(&c.to_string());
            let one = f.chars().count() == 1;
            let plain = f.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) || GREEK.contains(&*f) || f == "\u{3dd}";
            assert!(one && plain || styled(c).is_none(), "{c} folds to {f}");
        }
    }
}
