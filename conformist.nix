# posh's conformist overlay, merged with conformist.lib.presets.eng in
# flake.nix. The eng preset enables the language-agnostic eng-convention
# linters (eng-versioning, flake-*, justfile-*); this file picks posh's
# formatters and repo-specific tweaks. It replaces the former treefmt.nix —
# the formatter set below mirrors it. See conformist(7), conformist-nix(7),
# eng-versioning(7).
#
# This module is the single source: flake.nix's `nix fmt` / checks.formatting
# and the store-pinned conformist-pre-commit / conformist-repair git hooks all
# eval it directly (each hook bakes its own /nix/store config), so there is no
# committed conformist.toml to keep in sync.
{ ... }:
{
  # nixfmt formats the flake and the nix modules themselves.
  programs.nixfmt.enable = true;

  # Shell glue (scripts/*, .envrc). 2-space indent + -s simplify. NOTE: the treefmt config also passed -ci (case-indent); the
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
    # Build/CI artifacts and lockfiles.
    "result"
    "result-*"
    ".direnv/**"
    ".tmp/**"
    "*.lock"
    # Prose is out of scope for code formatters.
    "*.md"
    "flake.lock"
  ];
}
