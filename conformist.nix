# posh's conformist overlay, merged with conformist.lib.presets.eng in
# flake.nix. The eng preset enables the language-agnostic eng-convention
# linters (eng-versioning, flake-*, justfile-*); this file picks posh's
# formatters and repo-specific tweaks. It replaces the former treefmt.nix —
# the formatter set below mirrors it. See conformist(7), conformist-nix(7),
# eng-versioning(7).
#
# The generated config is committed as ./conformist.toml (just gen-conformist)
# so the bare `conformist --staged` pre-commit hook and `conformist --commit`
# repair hook — which take no --config-file and discover config by walking up
# the tree — find it. flake.nix's `nix fmt` / checks.formatting drive the same
# module directly, so the committed .toml and the nix wiring share one source.
{ ... }:
{
  # C++ reference tree (zz-mosh/src/**/*.cc, *.h). clang-format auto-discovers
  # zz-mosh/.clang-format (BasedOnStyle: Mozilla) by walking up from each file.
  programs.clang-format.enable = true;

  # Ship .clang-format into conformist's check sandbox. clang-format has no
  # native read-only mode, so `conformist check` copies each candidate file
  # into a sandbox, runs clang-format -i, and diffs — but the sandbox only
  # carries files the formatter declares via config-files. The upstream
  # clang-format module omits this (unlike rustfmt — conformist#28), so without
  # it the sandbox clang-format falls back to its LLVM default and spuriously
  # flags every Mozilla-styled zz-mosh file. Declaring it makes check mode agree
  # with repair mode (`nix fmt`) and with native `clang-format --dry-run`.
  settings.formatter.clang-format.config-files = [ ".clang-format" ];

  # nixfmt formats the flake and the nix modules themselves.
  programs.nixfmt.enable = true;

  # Shell glue (zz-mosh/scripts/*.sh, autogen.sh, .envrc). 2-space indent +
  # -s simplify. NOTE: the treefmt config also passed -ci (case-indent); the
  # conformist shfmt module does not expose that flag, so it is dropped — a
  # one-time reformat of the case statements in the shell glue.
  programs.shfmt = {
    enable = true;
    indent_size = 2;
  };

  # eng-versioning(7): the key normally derives from go.mod / Cargo.toml
  # [package].name, but posh's root Cargo.toml is a [workspace] with no
  # [package] table, so in the sandboxed checks.formatting lane (no .git, cwd =
  # /nix/store) derivation fails. Pin it explicitly to match version.env.
  linters.eng-versioning.key = "POSH_VERSION";

  settings.excludes = [
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
    # Prose and generated config are out of scope for code formatters.
    "*.md"
    "flake.lock"
    # The committed, generated config itself (just gen-conformist owns it).
    "conformist.toml"
  ];
}
