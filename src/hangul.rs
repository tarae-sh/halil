//! Hangul read as the keys it sits on, for the board's keys while a Korean
//! input method is on: it hands the terminal jamo and syllables, not letters.

/// The Dubeolsik key under each compatibility jamo, ㄱ (U+3131) to ㅣ
/// (U+3163): one key, or two for a jamo typed as two (ㅘ is ㅗ then ㅏ, a
/// final ㄳ is ㄱ then ㅅ). A doubled consonant is its key with Shift, as
/// ㄲ is `R`.
const KEYS: [&str; 51] = [
    "r", "R", "rt", "s", "sw", "sg", "e", "E", "f", "fr", "fa", "fq", "ft", "fx", "fv",
    "fg", // ㄱ..ㅀ
    "a", "q", "Q", "qt", "t", "T", "d", "w", "W", "c", "z", "x", "v", "g", // ㅁ..ㅎ
    "k", "o", "i", "O", "j", "p", "u", "P", "h", "hk", "ho", "hl", "y", "n", "nj", "np", "nl", "b",
    "m", "ml", "l", // ㅏ..ㅣ
];

/// A syllable's parts, as compatibility jamo: its 19 first consonants, 21
/// vowels and 27 last consonants, in Unicode's order.
const FIRST: &str = "ㄱㄲㄴㄷㄸㄹㅁㅂㅃㅅㅆㅇㅈㅉㅊㅋㅌㅍㅎ";
const VOWEL: &str = "ㅏㅐㅑㅒㅓㅔㅕㅖㅗㅘㅙㅚㅛㅜㅝㅞㅟㅠㅡㅢㅣ";
const LAST: &str = "ㄱㄲㄳㄴㄵㄶㄷㄹㄺㄻㄼㄽㄾㄿㅀㅁㅂㅄㅅㅆㅇㅈㅊㅋㅌㅍㅎ";

/// The keys `c` was typed with, in order, each with the jamo it stands for
/// (ㄳ gives `r` for ㄱ and `t` for ㅅ), when `c` is a compatibility jamo or
/// a composed syllable; `None` for anything else.
pub fn keys(c: char) -> Option<Vec<(char, char)>> {
    let mut out = Vec::new();
    for jamo in jamo_of(c)? {
        let keys = KEYS[jamo as usize - 0x3131];
        let stands_for = |key: char| match keys.len() {
            1 => jamo,
            _ => single(key).unwrap_or(jamo),
        };
        out.extend(keys.chars().map(|k| (k, stands_for(k))));
    }
    Some(out)
}

/// `c` as compatibility jamo: itself, or a syllable's two or three parts.
fn jamo_of(c: char) -> Option<Vec<char>> {
    match c as u32 {
        0x3131..=0x3163 => Some(vec![c]),
        0xAC00..=0xD7A3 => {
            let s = c as usize - 0xAC00;
            let mut parts = vec![
                FIRST.chars().nth(s / 588)?,
                VOWEL.chars().nth(s % 588 / 28)?,
            ];
            // 0 is no last consonant.
            let last = s % 28;
            if last > 0 {
                parts.push(LAST.chars().nth(last - 1)?);
            }
            Some(parts)
        }
        _ => None,
    }
}

/// The jamo typed with `key` alone.
fn single(key: char) -> Option<char> {
    KEYS.iter()
        .position(|k| k.len() == 1 && k.starts_with(key))
        .and_then(|i| char::from_u32(0x3131 + i as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(c: char) -> String {
        keys(c).unwrap().iter().map(|&(k, _)| k).collect()
    }

    #[test]
    fn jamo_read_as_their_dubeolsik_keys() {
        for (c, want) in [
            ('ㅓ', "j"),
            ('ㅗ', "h"),
            ('ㅏ', "k"),
            ('ㅣ', "l"),
            ('ㅎ', "g"),
            ('ㅂ', "q"),
            ('ㅁ', "a"),
            ('ㄷ', "e"),
            ('ㅇ', "d"),
            ('ㄴ', "s"),
            ('ㅅ', "t"),
            ('ㅌ', "x"),
            ('ㅛ', "y"),
            ('ㄲ', "R"),
            ('ㅒ', "O"),
            ('ㅘ', "hk"),
            ('ㅢ', "ml"),
            ('ㄳ', "rt"),
            ('ㅄ', "qt"),
        ] {
            assert_eq!(typed(c), want, "{c}");
        }
        // Every jamo has a key, and the table is in Unicode's order.
        for c in '\u{3131}'..='\u{3163}' {
            assert!(!typed(c).is_empty(), "{c}");
        }
        assert_eq!(single('j'), Some('ㅓ'));
        assert_eq!(single('R'), Some('ㄲ'));
    }

    #[test]
    fn a_syllable_is_its_jamos_keys_in_order() {
        assert_eq!(typed('어'), "dj");
        assert_eq!(typed('와'), "dhk");
        assert_eq!(typed('닭'), "ekfr");
        assert_eq!(typed('넋'), "sjrt");
        assert_eq!(typed('가'), "rk");
        assert_eq!(typed('힣'), "glg");
        assert_eq!(
            keys('넋').unwrap(),
            [('s', 'ㄴ'), ('j', 'ㅓ'), ('r', 'ㄱ'), ('t', 'ㅅ')],
            "each key with the jamo it stands for"
        );
    }

    #[test]
    fn anything_else_is_no_keys() {
        for c in ['a', 'J', ' ', '日', 'é', 'ㆍ', '\u{1100}'] {
            assert_eq!(keys(c), None, "{c}");
        }
    }
}
