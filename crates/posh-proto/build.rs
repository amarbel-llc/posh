// Flow version.env (POSH_VERSION) + the git SHA (POSH_GIT_SHA) into the crate
// as compile-time env vars, shared across every posh crate via posh-build so it
// cannot drift (github #71). See eng-versioning(7).
fn main() {
    posh_build::flow();
}
