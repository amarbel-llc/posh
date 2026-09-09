//! Look-alike glyphs for UNCONFIRMED local echo (FDR 0006 render-style
//! prototype): a predicted cell is drawn with a single-width homoglyph of
//! the typed character — a Greek, Cyrillic, Latin-extended, or other
//! visually similar letter — and the choice ROTATES over time, so a
//! prediction reads as the right shape while visibly "not settled", and
//! snaps to the real glyph the moment the server confirms it. The
//! alternative to an underline or a dim rendition: the marking is in the
//! glyph itself, not in an attribute a theme may not show.
//!
//! Only ASCII letters and digits have look-alikes; everything else (space,
//! punctuation, non-ASCII) is returned unchanged. Every variant is
//! single-width by [`posh_term::wcwidth`] (tested), so substitution never
//! reflows a row.

/// The rotation set for one ASCII character.
fn variants(ch: char) -> &'static [char] {
    match ch {
        'a' => &['ɑ', 'α', 'а'],
        'b' => &['ƅ', 'ɓ', 'ᖯ'],
        'c' => &['с', 'ϲ', 'ƈ'],
        'd' => &['ԁ', 'ɗ', 'đ'],
        'e' => &['е', 'ε', 'ɛ'],
        'f' => &['ƒ', 'ϝ', 'ғ'],
        'g' => &['ɡ', 'ց', 'ǥ'],
        'h' => &['һ', 'ɦ', 'ḥ'],
        'i' => &['і', 'ı', 'ɩ'],
        'j' => &['ϳ', 'ј', 'ɉ'],
        'k' => &['κ', 'ᴋ', 'ƙ'],
        'l' => &['ӏ', 'ɭ', 'ł'],
        'm' => &['м', 'ṃ', 'ɱ'],
        'n' => &['п', 'ո', 'ɳ'],
        'o' => &['о', 'σ', 'ο'],
        'p' => &['р', 'ρ', 'þ'],
        'q' => &['ԛ', 'զ', 'ɋ'],
        'r' => &['г', 'ᴦ', 'ɾ'],
        's' => &['ѕ', 'ꜱ', 'ʂ'],
        't' => &['τ', 'т', 'ŧ'],
        'u' => &['υ', 'ս', 'ʋ'],
        'v' => &['ѵ', 'ν', 'ⅴ'],
        'w' => &['ѡ', 'ա', 'ω'],
        'x' => &['х', 'χ', 'ᕁ'],
        'y' => &['у', 'γ', 'ý'],
        'z' => &['ᴢ', 'ƶ', 'ʐ'],
        'A' => &['Α', 'А', 'Λ'],
        'B' => &['Β', 'В', 'ß'],
        'C' => &['С', 'Ϲ', 'Ꮯ'],
        'D' => &['Ꭰ', 'Ð', 'Ď'],
        'E' => &['Ε', 'Е', 'Ɛ'],
        'F' => &['Ϝ', 'Ғ', 'Ƒ'],
        'G' => &['Ԍ', 'Ɠ', 'Ǥ'],
        'H' => &['Η', 'Н', 'Ⱨ'],
        'I' => &['Ι', 'І', 'Ɩ'],
        'J' => &['Ј', 'Ʝ', 'Ĵ'],
        'K' => &['Κ', 'К', 'Ƙ'],
        'L' => &['Ꮮ', 'Ⅼ', 'Ł'],
        'M' => &['Μ', 'М', 'Ϻ'],
        'N' => &['Ν', 'Ɲ', 'Ñ'],
        'O' => &['Ο', 'О', 'Ø'],
        'P' => &['Ρ', 'Р', 'Ƥ'],
        'Q' => &['Ԛ', 'Ǫ', 'Ɋ'],
        'R' => &['Ꭱ', 'Ʀ', 'Ŕ'],
        'S' => &['Ѕ', 'Ꮪ', 'Ş'],
        'T' => &['Τ', 'Т', 'Ŧ'],
        'U' => &['Ս', 'Ʊ', 'Ů'],
        'V' => &['Ѵ', 'Ⅴ', 'Ꮩ'],
        'W' => &['Ԝ', 'Ꮃ', 'Ŵ'],
        'X' => &['Χ', 'Х', 'Ⅹ'],
        'Y' => &['Υ', 'У', 'Ƴ'],
        'Z' => &['Ζ', 'Ꮓ', 'Ƶ'],
        '0' => &['Ο', 'О', 'Ø'],
        '1' => &['Ɩ', 'ӏ', 'Ⅰ'],
        '2' => &['Ƨ', 'ᒿ', 'Ϩ'],
        '3' => &['Ʒ', 'Ȝ', 'З'],
        '4' => &['Ꮞ', 'Ч', 'ч'],
        '5' => &['Ƽ', 'ƽ', 'Ș'],
        '6' => &['б', 'Ꮾ', 'ϐ'],
        '7' => &['ᒣ', '⁊', 'Ꮣ'],
        '8' => &['Ȣ', 'ȣ', '৪'],
        '9' => &['ǫ', '९', 'Ꮽ'],
        _ => &[],
    }
}

/// Whether `ch` has any look-alike at all (ASCII letters and digits).
pub fn has_lookalike(ch: char) -> bool {
    !variants(ch).is_empty()
}

/// The look-alike for `ch` at rotation step `tick` (any monotonic counter —
/// a frame number, or elapsed time divided by the rotation period). A
/// character without look-alikes is returned unchanged, so callers can
/// substitute every unconfirmed cell blindly.
pub fn lookalike(ch: char, tick: u64) -> char {
    let set = variants(ch);
    if set.is_empty() {
        ch
    } else {
        set[(tick % set.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALNUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";

    #[test]
    fn every_alnum_has_single_width_variants_that_differ_from_it() {
        for ch in ALNUM.chars() {
            let set = variants(ch);
            assert!(!set.is_empty(), "{ch:?} has no look-alikes");
            for &v in set {
                assert_eq!(posh_term::wcwidth(v), 1, "{ch:?} → {v:?} is not single-width");
                assert_ne!(v, ch, "{ch:?} look-alike is itself");
            }
        }
    }

    #[test]
    fn rotation_walks_the_set_and_wraps() {
        let set = variants('a');
        for tick in 0..(set.len() as u64 * 2) {
            assert_eq!(lookalike('a', tick), set[(tick as usize) % set.len()]);
        }
    }

    #[test]
    fn non_alnum_passes_through() {
        for ch in [' ', '-', '/', '~', 'é', '●'] {
            assert!(!has_lookalike(ch));
            assert_eq!(lookalike(ch, 7), ch);
        }
    }
}
