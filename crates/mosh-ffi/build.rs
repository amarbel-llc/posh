//! Compiles a minimal slice of mosh's C++ (terminal lib + the predictive-echo
//! overlay) plus the C-ABI shims into a static lib the Rust crate links against.
//!
//! Excludes `terminaldisplay*.cc` (the only ncurses dependency). The predictor
//! files (`terminaloverlay.cc`, `predictionlog.cc`) link light thanks to the
//! timing.h decouple (#5) — no crypto, no protobuf — and the predictor shim
//! provides the injected `Network::timestamp()` they need.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // crates/mosh-ffi -> workspace root -> zz-mosh (vendored mosh C++).
    let zz = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("zz-mosh");
    let term = zz.join("src/terminal");
    let frontend = zz.join("src/frontend");

    // Parser + emulator + framebuffer. No terminaldisplay*.cc (ncurses).
    let term_sources = [
        "parser.cc",
        "parserstate.cc",
        "parseraction.cc",
        "terminal.cc",
        "terminalframebuffer.cc",
        "terminalfunctions.cc",
        "terminaldispatcher.cc",
        "terminaluserinput.cc",
    ];
    // Predictive local echo (decoupled from network in #5).
    let frontend_sources = ["terminaloverlay.cc", "predictionlog.cc"];
    let shims = ["shim.cc", "predict_shim.cc"];

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++14") // mosh is pre-C++17; avoid removed throw()/register errors
        .include(&zz) // resolves `#include "src/.../..."`
        .include(&term) // sibling `#include "terminaldispatcher.h"` etc.
        .include(&frontend); // sibling frontend includes
    for s in &term_sources {
        build.file(term.join(s));
    }
    for s in &frontend_sources {
        build.file(frontend.join(s));
    }
    for s in &shims {
        build.file(manifest.join("csrc").join(s));
    }
    build.compile("moshterm");

    for s in &shims {
        println!("cargo:rerun-if-changed={}", manifest.join("csrc").join(s).display());
    }
    for s in &term_sources {
        println!("cargo:rerun-if-changed={}", term.join(s).display());
    }
    for s in &frontend_sources {
        println!("cargo:rerun-if-changed={}", frontend.join(s).display());
    }
}
