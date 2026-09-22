// Flow version.env (POSH_VERSION) + the git SHA (POSH_GIT_SHA) into the crate
// as compile-time env vars, plus the composed POSH_BUILD (`<version>+<sha>`)
// the `poshterity version` subcommand prints. The resolution logic is shared
// across every posh crate in posh-build so it cannot drift (github #71). See
// eng-versioning(7).
fn main() {
    posh_build::flow();
}
