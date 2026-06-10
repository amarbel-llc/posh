# treefmt-nix module config for posh.
#
# Wired into the flake at flake.nix via `treefmtEval`, exposed as
# `formatter.${system}` (`nix fmt`) and dropped into the devShell as the
# `treefmt` binary. `just codemod-fmt` calls `nix fmt` and routes through
# here; `just lint-fmt` builds the read-only `checks.formatting` gate.
{
  projectRootFile = "flake.nix";

  # C++ reference tree (zz-mosh/src/**/*.cc, *.h). clang-format
  # auto-discovers zz-mosh/.clang-format (BasedOnStyle: Mozilla).
  programs.clang-format.enable = true;

  programs.nixfmt.enable = true;

  # zz-mosh/scripts/*.sh, autogen.sh, and other shell glue.
  programs.shfmt = {
    enable = true;
    indent_size = 2;
  };

  settings.global.excludes = [
    # Vendored autoconf macros — upstream-maintained, not ours to reformat.
    "zz-mosh/m4/**"
    # Generated protobuf C++ (built into the tree as *.pb.cc / *.pb.h).
    "**/*.pb.cc"
    "**/*.pb.h"
    # Perl client script — no perl formatter wired up.
    "zz-mosh/scripts/mosh.pl"
    # Build/CI artifacts and lockfiles.
    "result"
    "result-*"
    ".direnv/**"
    ".tmp/**"
    "*.lock"
  ];
}
