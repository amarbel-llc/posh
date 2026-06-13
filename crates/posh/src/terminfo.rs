//! Minimal compiled-terminfo reader: just enough of term(5) to fetch the
//! enter/exit alternate-screen capabilities (smcup/rmcup) for the outer
//! terminal — mosh Display-init parity. No ncurses linkage: the binary
//! format is parsed directly (legacy 0432 and 32-bit-number 01036 magics),
//! the database is searched the way ncurses does ($TERMINFO, ~/.terminfo,
//! $TERMINFO_DIRS, then the standard system dirs, with both single-char
//! and Darwin-style hex subdirectories), and `$<..>` padding markers are
//! stripped from the result.
//!
//! Because the terminfo-DB search lives here, this module also owns the
//! remote session's terminal-env policy (posh#51): `resolve_term` picks a
//! $TERM the host's DB actually has, and `session_env` bundles it with the
//! forwarded $COLORTERM for the session shell.

use std::path::PathBuf;

/// String-capability table indices (ncurses Caps order).
const ENTER_CA_MODE: usize = 28; // smcup
const EXIT_CA_MODE: usize = 40; // rmcup

/// Hardcoded modern bracket used when no terminfo database answers:
/// every terminal posh targets implements DECSET 1049.
pub const FALLBACK_SMCUP: &[u8] = b"\x1b[?1049h";
pub const FALLBACK_RMCUP: &[u8] = b"\x1b[?1049l";

/// What the database said about $TERM's alternate-screen support.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The entry defines both smcup and rmcup.
    Found(Vec<u8>, Vec<u8>),
    /// The entry exists but defines no alternate screen (dumb, vt100):
    /// the terminal told us it can't, so don't try.
    NoAltScreen,
    /// No entry found (or unparsable): the database has no opinion.
    NoEntry,
}

/// The outer terminal's enter/exit alternate-screen bracket, or None when
/// takeover must be skipped.
///
/// - `POSH_NO_TERM_INIT` set non-empty (mosh `--no-init` parity): None.
/// - $TERM resolves to a terminfo entry: the entry's smcup/rmcup pair,
///   or None when the entry defines no alternate screen.
/// - No $TERM or no database (static deploys, sandboxes): the hardcoded
///   1049 pair — a missing database must not degrade the restore
///   guarantee on the modern terminals posh targets.
pub fn ca_mode_bracket() -> Option<(Vec<u8>, Vec<u8>)> {
    if std::env::var_os("POSH_NO_TERM_INIT").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let fallback = || (FALLBACK_SMCUP.to_vec(), FALLBACK_RMCUP.to_vec());
    let term = match std::env::var("TERM") {
        Ok(t) if !t.is_empty() => t,
        _ => return Some(fallback()),
    };
    match lookup_ca_mode(&term, &search_dirs()) {
        Lookup::Found(smcup, rmcup) => Some((smcup, rmcup)),
        Lookup::NoAltScreen => None,
        Lookup::NoEntry => Some(fallback()),
    }
}

/// Database directories in ncurses lookup order.
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(t) = std::env::var("TERMINFO") {
        if !t.is_empty() {
            dirs.push(PathBuf::from(t));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            dirs.push(PathBuf::from(home).join(".terminfo"));
        }
    }
    if let Ok(list) = std::env::var("TERMINFO_DIRS") {
        for d in list.split(':').filter(|d| !d.is_empty()) {
            dirs.push(PathBuf::from(d));
        }
    }
    for d in ["/etc/terminfo", "/lib/terminfo", "/usr/share/terminfo"] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// Candidate compiled-entry paths for `term` across `dirs`, in ncurses search
/// order: each dir × each subdir style. Linux uses a single-character
/// subdirectory; Darwin/macOS databases use the character's lowercase hex
/// code. Empty for an empty `term`. The one place the subdir layout lives.
fn candidate_paths<'a>(term: &'a str, dirs: &'a [PathBuf]) -> impl Iterator<Item = PathBuf> + 'a {
    let subdirs: Vec<String> = match term.chars().next() {
        Some(first) => vec![first.to_string(), format!("{:02x}", first as u32)],
        None => Vec::new(),
    };
    dirs.iter()
        .flat_map(move |dir| subdirs.clone().into_iter().map(move |sub| dir.join(sub).join(term)))
}

/// Finds and parses $TERM's compiled entry under `dirs`. The first file
/// that exists wins (ncurses behavior); a file that fails to parse is
/// treated as no entry.
pub fn lookup_ca_mode(term: &str, dirs: &[PathBuf]) -> Lookup {
    for path in candidate_paths(term, dirs) {
        if let Ok(bytes) = std::fs::read(&path) {
            return parse_ca_mode(&bytes).unwrap_or(Lookup::NoEntry);
        }
    }
    Lookup::NoEntry
}

/// Whether a compiled terminfo entry named `term` exists under `dirs`. Pure
/// path-presence check (the question curses' `setupterm` answers), independent
/// of whether the entry parses — distinct from `lookup_ca_mode`, which folds
/// an unreadable/malformed file into `NoEntry`.
pub fn entry_exists(term: &str, dirs: &[PathBuf]) -> bool {
    candidate_paths(term, dirs).any(|p| p.exists())
}

/// The $TERM to advertise to a remote session shell (posh#51). The session app
/// talks to posh-term (fixed kitty-parity capabilities), so the only constraint
/// on the *name* is that the host's terminfo DB has an entry curses can look
/// up. Prefer the client's forwarded $TERM, then xterm-256color, then xterm;
/// return the first the local DB actually has. Falls back to "xterm-256color"
/// even when the DB has none — better than an empty TERM (termenv still yields
/// ANSI256, and a DB-less host is degenerate). mosh forwards blindly and breaks
/// when the entry is absent; resolving avoids that.
pub fn resolve_term() -> String {
    resolve_term_in(&std::env::var("TERM").unwrap_or_default(), &search_dirs())
}

/// Pure core of `resolve_term`: the candidate preference and fallback, with
/// the client $TERM and search dirs injected so it is testable without
/// touching process env. `entry_exists("")` is false, so an unset client
/// candidate is skipped without a separate guard.
fn resolve_term_in(client: &str, dirs: &[PathBuf]) -> String {
    for cand in [client, "xterm-256color", "xterm"] {
        if entry_exists(cand, dirs) {
            return cand.to_string();
        }
    }
    "xterm-256color".to_string()
}

/// Environment a freshly spawned remote session shell should receive (posh#51):
/// a resolved TERM (never empty) plus the client's COLORTERM when it forwarded
/// a non-empty one. TERM has a sensible server-side default; COLORTERM does
/// not, so it is conditional. Owns the session-env policy so the server boot
/// path doesn't, and so the COLORTERM passthrough is testable.
pub fn session_env() -> Vec<(String, String)> {
    let mut env = vec![("TERM".to_string(), resolve_term())];
    if let Some(colorterm) = std::env::var("COLORTERM").ok().filter(|v| !v.is_empty()) {
        env.push(("COLORTERM".to_string(), colorterm));
    }
    env
}

/// Extracts smcup/rmcup from one compiled entry. None = malformed.
fn parse_ca_mode(b: &[u8]) -> Option<Lookup> {
    let u16le = |i: usize| -> Option<u16> {
        Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]))
    };
    let magic = u16le(0)?;
    // 0o432: legacy (16-bit numbers); 0o1036: 32-bit numbers. The numbers
    // section width is the only difference that matters here.
    let num_width = match magic {
        0o432 => 2,
        0o1036 => 4,
        _ => return None,
    };
    let name_size = u16le(2)? as usize;
    let bool_count = u16le(4)? as usize;
    let num_count = u16le(6)? as usize;
    let str_count = u16le(8)? as usize;
    let table_size = u16le(10)? as usize;

    let mut off = 12 + name_size + bool_count;
    if (name_size + bool_count) % 2 == 1 {
        off += 1; // numbers are short-aligned
    }
    off += num_count * num_width;
    let table_off = off + str_count * 2;
    if table_off + table_size > b.len() {
        return None;
    }

    let fetch = |idx: usize| -> Option<Vec<u8>> {
        if idx >= str_count {
            return None;
        }
        let raw = u16le(off + idx * 2)?;
        let str_off = raw as i16;
        if str_off < 0 || str_off as usize >= table_size {
            return None; // absent (-1), cancelled (-2), or out of table
        }
        let start = table_off + str_off as usize;
        let end = b[start..table_off + table_size]
            .iter()
            .position(|&c| c == 0)
            .map(|p| start + p)?;
        Some(strip_padding(&b[start..end]))
    };

    Some(match (fetch(ENTER_CA_MODE), fetch(EXIT_CA_MODE)) {
        (Some(smcup), Some(rmcup)) => Lookup::Found(smcup, rmcup),
        _ => Lookup::NoAltScreen,
    })
}

/// Removes terminfo `$<..>` padding/delay markers (tputs would interpret
/// them; written raw they'd print).
fn strip_padding(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'$' && s.get(i + 1) == Some(&b'<') {
            if let Some(close) = s[i + 2..].iter().position(|&c| c == b'>') {
                i += 2 + close + 1;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a compiled entry with the given (index, value) string caps.
    fn entry(magic: u16, name: &[u8], caps: &[(usize, &[u8])]) -> Vec<u8> {
        let str_count = caps.iter().map(|(i, _)| i + 1).max().unwrap_or(0);
        let mut offsets = vec![-1i16; str_count];
        let mut table: Vec<u8> = Vec::new();
        for (idx, val) in caps {
            offsets[*idx] = table.len() as i16;
            table.extend_from_slice(val);
            table.push(0);
        }
        let bool_count = 3usize; // arbitrary, exercises alignment
        let num_count = 2usize;
        let num_width = if magic == 0o1036 { 4 } else { 2 };
        let mut out = Vec::new();
        for v in [
            magic,
            name.len() as u16,
            bool_count as u16,
            num_count as u16,
            str_count as u16,
            table.len() as u16,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(name);
        out.extend_from_slice(&vec![0u8; bool_count]);
        if (name.len() + bool_count) % 2 == 1 {
            out.push(0);
        }
        out.extend_from_slice(&vec![0u8; num_count * num_width]);
        for o in &offsets {
            out.extend_from_slice(&o.to_le_bytes());
        }
        out.extend_from_slice(&table);
        out
    }

    #[test]
    fn parses_smcup_rmcup_in_both_magics() {
        for magic in [0o432u16, 0o1036] {
            let bytes = entry(
                magic,
                b"x|test entry",
                &[
                    (ENTER_CA_MODE, b"\x1b[?1049h\x1b[22;0;0t".as_slice()),
                    (EXIT_CA_MODE, b"\x1b[?1049l\x1b[23;0;0t".as_slice()),
                ],
            );
            assert_eq!(
                parse_ca_mode(&bytes),
                Some(Lookup::Found(
                    b"\x1b[?1049h\x1b[22;0;0t".to_vec(),
                    b"\x1b[?1049l\x1b[23;0;0t".to_vec()
                )),
                "magic {magic:o}"
            );
        }
    }

    #[test]
    fn odd_name_size_alignment() {
        // name + bools odd → a pad byte precedes the numbers section.
        let bytes = entry(
            0o432,
            b"x|odd", // 5 + 3 bools = 8, even; use 6 to go odd
            &[(ENTER_CA_MODE, b"A".as_slice()), (EXIT_CA_MODE, b"B".as_slice())],
        );
        assert!(matches!(parse_ca_mode(&bytes), Some(Lookup::Found(_, _))));
        let bytes = entry(
            0o432,
            b"x|odd!",
            &[(ENTER_CA_MODE, b"A".as_slice()), (EXIT_CA_MODE, b"B".as_slice())],
        );
        assert_eq!(
            parse_ca_mode(&bytes),
            Some(Lookup::Found(b"A".to_vec(), b"B".to_vec()))
        );
    }

    #[test]
    fn entry_without_alt_screen_reports_no_alt_screen() {
        // A dumb-like entry: strings exist but not smcup/rmcup.
        let bytes = entry(0o432, b"dumb|dumb", &[(5, b"\x1b[H\x1b[J".as_slice())]);
        assert_eq!(parse_ca_mode(&bytes), Some(Lookup::NoAltScreen));
        // No string section at all.
        let bytes = entry(0o432, b"dumb|dumb", &[]);
        assert_eq!(parse_ca_mode(&bytes), Some(Lookup::NoAltScreen));
    }

    #[test]
    fn padding_markers_are_stripped() {
        assert_eq!(strip_padding(b"\x1b[?1049h$<20/>x"), b"\x1b[?1049hx");
        assert_eq!(strip_padding(b"$<5>"), b"");
        // Unterminated marker passes through untouched.
        assert_eq!(strip_padding(b"a$<5"), b"a$<5");
    }

    #[test]
    fn garbage_is_rejected() {
        assert_eq!(parse_ca_mode(b""), None);
        assert_eq!(parse_ca_mode(b"\xff\xff\x00\x00"), None);
        // Header promising more than the file holds.
        let mut bytes = entry(0o432, b"x|t", &[(ENTER_CA_MODE, b"A".as_slice())]);
        bytes.truncate(14);
        assert_eq!(parse_ca_mode(&bytes), None);
    }

    #[test]
    fn lookup_walks_dirs_and_subdir_styles() {
        let tmp = std::env::temp_dir().join(format!("posh-ti-{}", std::process::id()));
        // Hex-style subdir ('z' = 7a), as on Darwin databases.
        let hexdir = tmp.join("7a");
        std::fs::create_dir_all(&hexdir).unwrap();
        let bytes = entry(
            0o432,
            b"ztest|test",
            &[(ENTER_CA_MODE, b"E".as_slice()), (EXIT_CA_MODE, b"X".as_slice())],
        );
        std::fs::write(hexdir.join("ztest"), &bytes).unwrap();
        assert_eq!(
            lookup_ca_mode("ztest", &[tmp.clone()]),
            Lookup::Found(b"E".to_vec(), b"X".to_vec())
        );
        assert_eq!(lookup_ca_mode("missing", &[tmp.clone()]), Lookup::NoEntry);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn entry_exists_is_a_pure_path_check() {
        let tmp = std::env::temp_dir().join(format!("posh-ti-ex-{}", std::process::id()));
        // Single-char subdir ('x'), and a present-but-empty file (existence is
        // independent of whether it parses as a real terminfo entry).
        let dir = tmp.join("x");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("xterm-256color"), b"not a real entry").unwrap();
        let dirs = [tmp.clone()];
        assert!(entry_exists("xterm-256color", &dirs));
        assert!(!entry_exists("xterm-kitty", &dirs));
        assert!(!entry_exists("", &dirs));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_term_in_prefers_client_then_falls_through_to_present_entry() {
        let tmp = std::env::temp_dir().join(format!("posh-ti-rt-{}", std::process::id()));
        let dir = tmp.join("x");
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = [tmp.clone()];

        // Only plain xterm present: a client TERM the DB lacks falls through
        // past the missing xterm-256color candidate to xterm.
        std::fs::write(dir.join("xterm"), b"x").unwrap();
        assert_eq!(resolve_term_in("xterm-fancy", &dirs), "xterm");

        // Client TERM present → preferred over the later candidates.
        std::fs::write(dir.join("xterm-kitty"), b"x").unwrap();
        assert_eq!(resolve_term_in("xterm-kitty", &dirs), "xterm-kitty");

        // Empty client (unset $TERM) is skipped, not matched.
        assert_eq!(resolve_term_in("", &dirs), "xterm"); // xterm-256color absent

        // DB-less dirs: hardcoded fallback rather than empty.
        assert_eq!(resolve_term_in("whatever", &[]), "xterm-256color");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Opportunistic cross-check against the real database: validates the
    /// 28/40 capability indices against ncurses-compiled data wherever a
    /// database exists (skips silently in hermetic sandboxes).
    #[test]
    fn real_database_xterm_smcup_is_1049() {
        match lookup_ca_mode("xterm-256color", &search_dirs()) {
            Lookup::Found(smcup, rmcup) => {
                assert!(
                    smcup.starts_with(b"\x1b[?1049h"),
                    "unexpected smcup: {:?}",
                    String::from_utf8_lossy(&smcup)
                );
                assert!(
                    rmcup.starts_with(b"\x1b[?1049l"),
                    "unexpected rmcup: {:?}",
                    String::from_utf8_lossy(&rmcup)
                );
            }
            // No database in this environment (nix sandbox): nothing to
            // cross-check.
            Lookup::NoEntry => {}
            Lookup::NoAltScreen => panic!("xterm-256color must define smcup/rmcup"),
        }
    }
}
