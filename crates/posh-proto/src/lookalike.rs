//! Look-alike glyphs for UNCONFIRMED local echo (FDR 0006 render-style
//! prototype): a predicted cell is drawn with a single-width look-alike of
//! the typed character — its case swap, a Greek / Cyrillic / Latin-extended
//! homoglyph, or a symbol of the same silhouette (`|` for `l`, `†` for `t`,
//! `×` for `x`, `→` for `-`) — and the choice CHANGES AT RANDOM over time, so a
//! prediction reads as the right shape while visibly "not settled", and
//! snaps to the real glyph the moment the server confirms it. The
//! alternative to an underline or a dim rendition: the marking is in the
//! glyph itself, not in an attribute a theme may not show.
//!
//! ASCII letters, digits, and the common punctuation have look-alikes;
//! everything else (space, control, non-ASCII) is returned unchanged. Every
//! variant is single-width by [`posh_term::wcwidth`] (tested), so
//! substitution never reflows a row.

/// The rotation set for one ASCII character. Letters lead with their case
/// swap so it is always in the rotation.
fn variants(ch: char) -> &'static [char] {
    match ch {
        'a' => &['A', 'ɑ', 'α', 'а', 'ä', 'ª', 'λ'],
        'b' => &['B', 'ƅ', 'ɓ', 'ᖯ', 'Ь', 'ß', 'þ'],
        'c' => &['C', 'с', 'ϲ', 'ƈ', 'ç', '¢', 'ↄ'],
        'd' => &['D', 'ԁ', 'ɗ', 'đ', 'ð', 'ⅾ', 'ժ'],
        'e' => &['E', 'е', 'ε', 'ɛ', 'ë', 'є', '€'],
        'f' => &['F', 'ƒ', 'ϝ', 'ғ', 'ſ', 'ʄ', 'ƭ'],
        'g' => &['G', 'ɡ', 'ց', 'ǥ', 'ɠ', 'ḡ', '9'],
        'h' => &['H', 'һ', 'ɦ', 'ḥ', 'ħ', 'ђ', 'ɧ'],
        'i' => &['I', 'і', 'ı', 'ɩ', 'ï', 'ⅰ', '¡', '|'],
        'j' => &['J', 'ϳ', 'ј', 'ɉ', 'ʝ', 'ĵ', 'ǰ'],
        'k' => &['K', 'κ', 'ᴋ', 'ƙ', 'ķ', 'к', 'ⱪ'],
        'l' => &['L', 'ӏ', 'ɭ', 'ł', '|', 'ǀ', 'ⅼ', '1'],
        'm' => &['M', 'м', 'ṃ', 'ɱ', 'ⅿ', 'ɯ', 'ʍ'],
        'n' => &['N', 'п', 'ո', 'ɳ', 'ñ', 'ή', 'ŋ'],
        'o' => &['O', 'о', 'σ', 'ο', 'ö', 'ø', '0', '°'],
        'p' => &['P', 'р', 'ρ', 'þ', 'ƥ', 'ṗ', 'ք'],
        'q' => &['Q', 'ԛ', 'զ', 'ɋ', 'ʠ', 'ǫ', 'ϙ'],
        'r' => &['R', 'г', 'ᴦ', 'ɾ', 'ř', 'ʀ', 'ɼ'],
        's' => &['S', 'ѕ', 'ꜱ', 'ʂ', 'ş', '§', '5'],
        't' => &['T', 'τ', 'т', 'ŧ', '†', 'ţ', '+'],
        'u' => &['U', 'υ', 'ս', 'ʋ', 'ü', 'µ', 'ʊ'],
        'v' => &['V', 'ѵ', 'ν', 'ⅴ', '√', 'ʌ', 'ⱱ'],
        'w' => &['W', 'ѡ', 'ա', 'ω', 'ŵ', 'ѿ', 'ɰ'],
        'x' => &['X', 'х', 'χ', 'ᕁ', '×', '✕', 'ⅹ'],
        'y' => &['Y', 'у', 'γ', 'ý', 'ÿ', '¥', 'ʏ'],
        'z' => &['Z', 'ᴢ', 'ƶ', 'ʐ', 'ž', 'ζ', '2'],
        'A' => &['a', 'Α', 'А', 'Λ', 'Ä', 'Å', '∆'],
        'B' => &['b', 'Β', 'В', 'ß', 'Ɓ', 'Ƀ', '8'],
        'C' => &['c', 'С', 'Ϲ', 'Ꮯ', 'Ç', 'Ↄ', 'ℂ'],
        'D' => &['d', 'Ꭰ', 'Ð', 'Ď', 'Đ', 'ⅅ', 'Ɗ'],
        'E' => &['e', 'Ε', 'Е', 'Ɛ', 'Ë', 'Σ', '∃'],
        'F' => &['f', 'Ϝ', 'Ғ', 'Ƒ', 'Ⅎ', '₣', 'Ḟ'],
        'G' => &['g', 'Ԍ', 'Ɠ', 'Ǥ', 'Ģ', 'Ĝ', '6'],
        'H' => &['h', 'Η', 'Н', 'Ⱨ', 'Ħ', 'Ĥ', 'Ḥ'],
        'I' => &['i', 'Ι', 'І', 'Ɩ', '|', 'Ï', 'Ⅰ', '1'],
        'J' => &['j', 'Ј', 'Ʝ', 'Ĵ', 'Ɉ', 'ᒍ', 'ᒚ'],
        'K' => &['k', 'Κ', 'К', 'Ƙ', 'Ķ', 'Ⱪ', 'Ḱ'],
        'L' => &['l', 'Ꮮ', 'Ⅼ', 'Ł', 'Ŀ', 'Ⱡ', 'Ļ'],
        'M' => &['m', 'Μ', 'М', 'Ϻ', 'Ⅿ', 'Ḿ', 'Ṁ'],
        'N' => &['n', 'Ν', 'Ɲ', 'Ñ', 'И', 'Ň', 'Ṅ'],
        'O' => &['o', 'Ο', 'О', 'Ø', '0', 'Ö', '⊙'],
        'P' => &['p', 'Ρ', 'Р', 'Ƥ', 'Þ', '₽', 'Ṕ'],
        'Q' => &['q', 'Ԛ', 'Ǫ', 'Ɋ', 'ℚ', 'Ǭ'],
        'R' => &['r', 'Ꭱ', 'Ʀ', 'Ŕ', 'Я', 'ℝ', 'Ř'],
        'S' => &['s', 'Ѕ', 'Ꮪ', 'Ş', '§', 'Ș', '5'],
        'T' => &['t', 'Τ', 'Т', 'Ŧ', '†', 'Ţ', 'Ⱦ'],
        'U' => &['u', 'Ս', 'Ʊ', 'Ů', 'Ü', '∪', 'Ų'],
        'V' => &['v', 'Ѵ', 'Ⅴ', 'Ꮩ', '√', 'Ṽ', 'Ʌ'],
        'W' => &['w', 'Ԝ', 'Ꮃ', 'Ŵ', 'Ш', 'Ẅ', 'Ѡ'],
        'X' => &['x', 'Χ', 'Х', 'Ⅹ', '×', '✕', 'Ẋ'],
        'Y' => &['y', 'Υ', 'У', 'Ƴ', '¥', 'Ÿ', 'Ý'],
        'Z' => &['z', 'Ζ', 'Ꮓ', 'Ƶ', 'Ž', 'ℤ', '2'],
        '0' => &['O', 'o', 'Ο', 'О', 'Ø', '∅', '°'],
        '1' => &['l', 'I', '|', 'Ɩ', 'ӏ', 'Ⅰ', '¹'],
        '2' => &['Ƨ', 'ᒿ', 'Ϩ', 'z', 'Z', '²', 'ƻ'],
        '3' => &['Ʒ', 'Ȝ', 'З', 'Ɛ', '³', 'ʒ', 'E'],
        '4' => &['Ꮞ', 'Ч', 'ч', 'Ꭴ', 'ᔦ', 'Ⴁ', 'A'],
        '5' => &['Ƽ', 'ƽ', 'Ș', 'S', 's', 'Ƨ', '§'],
        '6' => &['б', 'Ꮾ', 'ϐ', 'G', 'ƃ', 'Ƅ', 'ϭ'],
        '7' => &['ᒣ', '⁊', 'Ꮣ', 'T', 'ʇ', '⁷', '⅂'],
        '8' => &['Ȣ', 'ȣ', '৪', 'B', '∞', '⁸', 'ϐ'],
        '9' => &['ǫ', '९', 'Ꮽ', 'g', 'q', '⁹', 'ց'],
        '-' => &['–', '—', '→', '−', '‐', '⁃'],
        '_' => &['‗', '⎯', '‿', '⁀', '▁'],
        '|' => &['¦', '∣', 'ǀ', 'l', 'I', '1'],
        '/' => &['∕', '⁄', '⟋', '╱', '⧸'],
        '\\' => &['∖', '⧵', '╲', '⟍', '⧹'],
        '.' => &['·', '∙', '•', '․', '˙'],
        ',' => &['‚', 'ˏ', '¸', 'ʻ'],
        ':' => &['∶', '⁚', 'ː', '⁝'],
        ';' => &['\u{37e}', '⁏', '⸵'],
        '=' => &['≡', '⁼', '═', '⩵', '≈'],
        '+' => &['†', '✝', '⊕', '✚', 't'],
        '*' => &['∗', '✱', '⁎', '⋆', '✳'],
        '<' => &['‹', '⟨', '≺', '˂', '≪'],
        '>' => &['›', '⟩', '≻', '˃', '≫'],
        '(' => &['❨', '⁽', '⟮', '⸨'],
        ')' => &['❩', '⁾', '⟯', '⸩'],
        '[' => &['⁅', '⟦', '❲', '⸢'],
        ']' => &['⁆', '⟧', '❳', '⸥'],
        '{' => &['❴', '⦃', '⁅'],
        '}' => &['❵', '⦄', '⁆'],
        '~' => &['∼', '˜', '⁓', '≈', '∽'],
        '!' => &['ǃ', '¡', '│', 'ⵑ'],
        '?' => &['¿', 'ʔ', '⁇', 'ɂ'],
        '#' => &['♯', '⌗', '⩩', '≠'],
        '$' => &['₴', 'Ꞩ', 'ꞩ', 'Ş', 'S'],
        '%' => &['‰', '⁒', '℀', '℅'],
        '&' => &['⅋', 'ꝸ', '₰', 'ȣ'],
        '@' => &['⍟', 'ª', 'ɑ', 'ᵃ'],
        '^' => &['ˆ', '˄', '⌃', '∧', 'ᶺ'],
        '\'' => &['’', 'ʼ', '′', '‘', 'ˈ'],
        '"' => &['“', '″', 'ʺ', '”', '˝'],
        '`' => &['‘', 'ˋ', '´', 'ˊ'],
        _ => &[],
    }
}

/// Whether `ch` has any look-alike at all.
pub fn has_lookalike(ch: char) -> bool {
    !variants(ch).is_empty()
}

/// splitmix64: a cheap, well-mixed hash so the pick looks random.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The look-alike for `ch` at `seed` — a RANDOM pick from its set, not a
/// walk: hash the seed (a rotation tick, ideally salted per cell with
/// [`cell_seed`] so neighbours don't move in lockstep) and index by it. A
/// character without look-alikes is returned unchanged, so callers can
/// substitute every unconfirmed cell blindly. May repeat the previous
/// tick's pick; [`next_lookalike`] never does.
pub fn lookalike(ch: char, seed: u64) -> char {
    let set = variants(ch);
    if set.is_empty() {
        ch
    } else {
        set[(mix(seed) % set.len() as u64) as usize]
    }
}

/// A random look-alike for `ch` that is NOT `current` (what the cell shows
/// now), so every tick visibly changes the glyph and a stall can't be
/// mistaken for the server settling it. `current = None` (or a glyph not
/// in the set) is an unconstrained pick.
pub fn next_lookalike(ch: char, current: Option<char>, seed: u64) -> char {
    let set = variants(ch);
    let n = set.len();
    if n == 0 {
        return ch;
    }
    let Some(cur) = current.and_then(|c| set.iter().position(|&v| v == c)) else {
        return set[(mix(seed) % n as u64) as usize];
    };
    if n == 1 {
        return set[0];
    }
    // Pick among the n-1 entries that are not `cur`.
    let mut idx = (mix(seed) % (n as u64 - 1)) as usize;
    if idx >= cur {
        idx += 1;
    }
    set[idx]
}

/// A per-cell salt for [`lookalike`]'s seed, so cells at different
/// positions pick independently at the same tick.
pub fn cell_seed(tick: u64, cell: u64) -> u64 {
    tick.wrapping_add(cell.wrapping_mul(0x9E37_79B9_7F4A_7C15))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COVERED: &str =
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_|/\\.,:;=+*<>()[]{}~!?#$%&@^'\"`";

    #[test]
    fn every_covered_char_has_single_width_variants_that_differ_from_it() {
        for ch in COVERED.chars() {
            let set = variants(ch);
            assert!(set.len() >= 3, "{ch:?} has too few look-alikes ({})", set.len());
            for &v in set {
                assert_eq!(posh_term::wcwidth(v), 1, "{ch:?} → {v:?} is not single-width");
                assert_ne!(v, ch, "{ch:?} look-alike is itself");
            }
        }
    }

    #[test]
    fn letters_rotate_through_their_case_swap() {
        for ch in ('a'..='z').chain('A'..='Z') {
            let swap = if ch.is_ascii_lowercase() {
                ch.to_ascii_uppercase()
            } else {
                ch.to_ascii_lowercase()
            };
            assert!(variants(ch).contains(&swap), "{ch:?} lacks its case swap");
        }
    }

    #[test]
    fn picks_are_random_never_repeat_consecutively_and_cover_the_set() {
        for ch in ['a', 'X', '7', '-'] {
            let set = variants(ch);
            let mut seen = std::collections::HashSet::new();
            let mut shown = None;
            for tick in 0..500u64 {
                let v = next_lookalike(ch, shown, tick);
                assert!(set.contains(&v));
                assert_ne!(Some(v), shown, "{ch:?} repeated {v:?} at tick {tick}");
                shown = Some(v);
                seen.insert(v);
            }
            assert_eq!(seen.len(), set.len(), "{ch:?}: every variant should show up");
        }
        // The plain pick is in-set and not a fixed walk.
        let plain: Vec<char> = (0..20).map(|t| lookalike('a', t)).collect();
        assert!(plain.iter().all(|v| variants('a').contains(v)));
        assert_ne!(plain, (0..20).map(|t| variants('a')[t as usize % variants('a').len()]).collect::<Vec<_>>());
        // Neighbouring cells at the same tick do not move in lockstep.
        let same_tick: Vec<char> = (0..8).map(|cell| lookalike('e', cell_seed(3, cell))).collect();
        assert!(same_tick.iter().any(|&v| v != same_tick[0]));
    }

    #[test]
    fn sets_have_no_duplicates() {
        for ch in COVERED.chars() {
            let set = variants(ch);
            let unique: std::collections::HashSet<char> = set.iter().copied().collect();
            assert_eq!(unique.len(), set.len(), "{ch:?} has a duplicate look-alike: {set:?}");
        }
    }

    #[test]
    fn uncovered_chars_pass_through() {
        for ch in [' ', '\t', 'é', '●', 'ß'] {
            assert!(!has_lookalike(ch));
            assert_eq!(lookalike(ch, 7), ch);
        }
    }
}
