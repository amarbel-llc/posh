//! Byte-for-byte characterization of mosh's terminal emulator, driven through
//! the FFI shim (task #4, terminal slice; see
//! docs/plans/2026-06-16-mosh-characterization-harness-design.md).
//!
//! Each fixture is an escape-encoded VT script (`tests/fixtures/<name>.in`) fed
//! to mosh's `Terminal::Emulator` at a declared size; the rendered grid is
//! compared to a blessed golden (`<name>.grid`). Set `MOSH_FFI_BLESS=1` (via
//! `just debug-mosh-bless`) to (re)generate the goldens; the normal dev loop
//! (`just debug-cargo test -p mosh-ffi`) asserts against them.
//!
//! Deterministic by construction: no clock, no network, no timing — a fixed
//! script always renders the same grid. That is what lets these goldens guard a
//! later behavior-preserving refactor.

use std::path::PathBuf;

use mosh_ffi::MoshTerminal;

struct Case {
    name: &'static str,
    cols: u16,
    rows: u16,
}

const CASES: &[Case] = &[
    Case { name: "plain", cols: 24, rows: 6 },
    Case { name: "cursor", cols: 24, rows: 6 },
    Case { name: "erase", cols: 24, rows: 6 },
    Case { name: "scroll", cols: 12, rows: 4 },
    Case { name: "tabs", cols: 24, rows: 3 },
];

/// Decodes a fixture script: `\n \r \t \e \\` and `\xNN`; everything else is
/// literal. A trailing file newline (editor artifact) is stripped first, since
/// intended newlines are written as `\r\n` in the script.
fn decode(script: &str) -> Vec<u8> {
    let b = script.trim_end_matches('\n').as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            match b[i + 1] {
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                }
                b't' => {
                    out.push(b'\t');
                    i += 2;
                }
                b'e' => {
                    out.push(0x1b);
                    i += 2;
                }
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                b'x' if i + 3 < b.len() => {
                    let hi = (b[i + 2] as char).to_digit(16);
                    let lo = (b[i + 3] as char).to_digit(16);
                    match (hi, lo) {
                        (Some(h), Some(l)) => {
                            out.push((h * 16 + l) as u8);
                            i += 4;
                        }
                        _ => {
                            out.push(b[i]);
                            i += 1;
                        }
                    }
                }
                _ => {
                    out.push(b[i]);
                    i += 1;
                }
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

/// Per-row trailing whitespace trimmed so goldens are stable against editors /
/// formatters that strip line-end spaces. `render()` itself stays faithful.
fn normalize(grid: &str) -> String {
    grid.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn terminal_characterization_matches_goldens() {
    let bless = std::env::var_os("MOSH_FFI_BLESS").is_some();
    let dir = fixtures_dir();
    let mut failures = Vec::new();

    for case in CASES {
        let script = std::fs::read_to_string(dir.join(format!("{}.in", case.name)))
            .unwrap_or_else(|e| panic!("read {}.in: {e}", case.name));
        let mut term = MoshTerminal::new(case.cols, case.rows);
        term.feed(&decode(&script));
        let actual = normalize(&term.render());

        let golden_path = dir.join(format!("{}.grid", case.name));
        if bless {
            std::fs::write(&golden_path, format!("{actual}\n"))
                .unwrap_or_else(|e| panic!("bless {}.grid: {e}", case.name));
            continue;
        }

        let expected = std::fs::read_to_string(&golden_path).unwrap_or_else(|e| {
            panic!(
                "read {}.grid: {e} (run `just debug-mosh-bless` to generate)",
                case.name
            )
        });
        let expected = expected.strip_suffix('\n').unwrap_or(&expected);
        if actual != expected {
            failures.push(format!(
                "--- {} ---\nexpected:\n{expected}\nactual:\n{actual}",
                case.name
            ));
        }
    }

    if bless {
        eprintln!("blessed {} goldens", CASES.len());
        return;
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n\n"));
}
