{
  description = "charm-tui-poc: a bubbletea TUI hosted on posh-term (throwaway POC)";

  # Pinned to the same nixpkgs rev posh's flake.lock already has cached, so
  # `nix develop` here does not fetch a fresh nixpkgs.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/d233902339c02a9c334e7e593de68855ad26c4cb";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        # The repo devShell has no Go toolchain (posht builds Go only via
        # buildGoModule). This isolated shell brings it for the POC dev-loop.
        packages = [ pkgs.go ];
      };
    };
}
