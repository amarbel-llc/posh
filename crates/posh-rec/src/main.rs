//! The `posh-rec` binary: a thin shim over [`posh_rec::cli`]. The same entry
//! point is reachable as `posh rec ...` on the posh binary (see
//! crates/posh/src/main.rs).

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = posh_rec::cli::run(&args) {
        eprintln!("posh-rec: {e}");
        std::process::exit(1);
    }
}
