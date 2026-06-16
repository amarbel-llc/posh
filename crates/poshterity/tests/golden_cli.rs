//! End-to-end `bless` -> `assert` round-trip via the real binary, plus a
//! deliberate regression that must fail with a diff.

use std::process::Command;

// "red" (cols 0-2) in SGR 31 = indexed 1, then " ok", and a marker.
const DOC: &str = "{\"version\":2,\"width\":20,\"height\":2,\"poshterity\":{\"v\":1,\"emu_rev\":\"0.1.0\"}}\n\
                   [0.0,\"o\",\"\\u001b[31mred\\u001b[0m ok\"]\n\
                   [0.1,\"m\",\"shown\"]\n";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_poshterity")
}

#[test]
fn bless_then_assert_round_trips_and_regression_fails() {
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    let castx = dir.join(format!("poshterity-golden-{pid}.castx"));
    let golden = dir.join(format!("poshterity-golden-{pid}.golden"));
    std::fs::write(&castx, DOC).unwrap();
    let (c, g) = (castx.to_str().unwrap(), golden.to_str().unwrap());

    let bless = Command::new(bin())
        .args(["bless", c, "--golden", g, "--at", "shown"])
        .status()
        .unwrap();
    assert!(bless.success(), "bless failed");

    let pass = Command::new(bin())
        .args(["assert", c, "--golden", g, "--at", "shown"])
        .status()
        .unwrap();
    assert!(pass.success(), "assert should pass against its own bless");

    // Simulate a color regression in the stored golden (red -> green slot).
    let mutated = std::fs::read_to_string(&golden).unwrap().replace("fg=i1", "fg=i2");
    std::fs::write(&golden, mutated).unwrap();

    let fail = Command::new(bin())
        .args(["assert", c, "--golden", g, "--at", "shown"])
        .output()
        .unwrap();
    assert!(!fail.status.success(), "assert must fail on a mismatch");
    assert!(
        String::from_utf8_lossy(&fail.stderr).contains("mismatch"),
        "expected a mismatch diff on stderr"
    );

    let _ = std::fs::remove_file(&castx);
    let _ = std::fs::remove_file(&golden);
}
