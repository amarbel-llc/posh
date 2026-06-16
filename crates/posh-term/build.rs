// Flow version.env (POSH_VERSION) + the git SHA (POSH_GIT_SHA) into the crate
// as compile-time env vars; runtime reads env!("POSH_VERSION") /
// env!("POSH_GIT_SHA") via posh_term::version() / git_rev() / emu_rev(). The
// resolution logic is shared across every posh crate in posh-build so it cannot
// drift (github #71). See eng-versioning(7).
fn main() {
    posh_build::flow();
}
