{
  description = "Mosh: the mobile shell — remote terminal over UDP with roaming and local echo";

  inputs = {
    # Fork of upstream nixpkgs with the amarbel-llc package additions
    # (the eng fleet's cached overlay). See eng-nix(7) THE NIXPKGS-MASTER
    # INPUT and eng-versioning(7).
    igloo.url = "github:amarbel-llc/igloo";
    utils.url = "https://flakehub.com/f/numtide/flake-utils/0.1.102";
    nixpkgs-master.url = "github:NixOS/nixpkgs/d233902339c02a9c334e7e593de68855ad26c4cb";

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "igloo";
    };
  };

  # The `...` ellipsis is load-bearing: nix calls `outputs` with `self`
  # plus every resolved input, so a strict `{ self, igloo, ... }` without
  # it silently breaks the flake the moment a new input is added. See
  # eng-nix(7) FLAKE OUTPUTS DESTRUCTURING.
  outputs =
    {
      self,
      igloo,
      utils,
      treefmt-nix,
      ...
    }:
    utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import igloo {
          inherit system;
          overlays = [ igloo.overlays.default ];
        };
        inherit (pkgs) lib;

        # Single source of truth for the user-visible version; matches
        # AC_INIT([mosh],[1.4.0]) in configure.ac. The autotools build
        # derives its own VERSION.stamp from git-describe (absent in the
        # nix sandbox, so it falls back to "mosh 1.4.0"); this literal is
        # only for the derivation `version` attr. See eng-versioning(7).
        moshVersion = "1.4.0";

        # Build-time toolchain: autoreconf stack + protoc + pkg-config, and
        # perl because scripts/Makefile.am runs `perl -Mdiagnostics -c` on
        # mosh.pl while generating the `mosh` client script.
        moshNativeBuildInputs = with pkgs; [
          autoconf
          automake
          pkg-config
          protobuf
          perl
          makeWrapper
        ];

        # Link/runtime libraries. protobuf appears here AND in
        # nativeBuildInputs on purpose: configure.ac hard-errors if protoc
        # and the protobuf headers/libs are different versions, so both must
        # come from the same package. libutempter is Linux-only (utmp
        # entries); --with-utempter=check only warns when absent, but we pin
        # it so utmp recording works.
        moshBuildInputs =
          with pkgs;
          [
            protobuf
            ncurses
            zlib
            openssl
          ]
          ++ lib.optional stdenv.hostPlatform.isLinux libutempter;

        mosh = pkgs.stdenv.mkDerivation {
          pname = "mosh";
          version = moshVersion;

          src = ./.;

          nativeBuildInputs = moshNativeBuildInputs;
          buildInputs = moshBuildInputs;

          # autogen.sh is `exec autoreconf -fi`; run it to generate
          # ./configure from configure.ac + the vendored m4/ macros.
          preConfigure = ''
            ./autogen.sh
          '';

          # openssl is the portable default crypto backend. We pin it
          # explicitly rather than letting configure auto-detect Apple
          # CommonCrypto on darwin, so both platforms build the same
          # cryptography path. README "Advice to distributors".
          configureFlags = [
            "--with-crypto-library=openssl"
          ];

          # `make check` builds and runs the src/tests suite. Some tests
          # drive a pty / use timing that the stricter nix build sandbox on
          # darwin can choke on (same reason piggy and ssh-agent-mux set
          # `doCheck = !isDarwin`). Start with checks ON everywhere; this is
          # flipped to a darwin opt-out only if a specific test proves
          # un-sandboxable.
          doCheck = true;

          # local.test / mouse-alternate-scroll.test exec the generated
          # scripts/mosh, which inherits mosh.pl's `#!/usr/bin/env perl`
          # shebang — absent in the sandbox, so inpty's execve fails and
          # both tests FAIL instead of asserting. Patch the shebang so the
          # --local round-trip tests really run in this lane. github #4.
          preCheck = ''
            patchShebangs scripts/mosh
          '';

          # The installed `mosh` client is a Perl script (generated from
          # scripts/mosh.pl). Wrap it so perl is on PATH at runtime
          # regardless of the user's environment — mirrors piggy's
          # makeWrapper runtime-dep pinning.
          postFixup = ''
            if [ -e "$out/bin/mosh" ]; then
              wrapProgram "$out/bin/mosh" \
                --prefix PATH : ${lib.makeBinPath [ pkgs.perl ]}
            fi
          '';

          meta = with lib; {
            description = "Mobile shell: remote terminal over UDP supporting roaming and intermittent connectivity";
            homepage = "https://mosh.org";
            license = licenses.gpl3Plus;
            mainProgram = "mosh";
            platforms = platforms.linux ++ platforms.darwin;
          };
        };

        # Tree-wide formatter: clang-format (C++) + nixfmt + shfmt under one
        # wrapper. Exposed as `formatter.${system}` (so `nix fmt` works) and
        # dropped into the devShell. See ./treefmt.nix.
        treefmtEval = treefmt-nix.lib.evalModule pkgs ./treefmt.nix;
      in
      {
        packages = {
          default = mosh;
          mosh = mosh;
        };

        checks = {
          # Read-only formatting gate: builds in /nix/store off a source
          # snapshot, runs treefmt, fails if any file would change. Driven
          # by `just lint-fmt` and surfaced under `nix flake check`.
          formatting = treefmtEval.config.build.check self;
        };

        formatter = treefmtEval.config.build.wrapper;

        devShells.default = pkgs.mkShell {
          packages =
            moshNativeBuildInputs
            ++ moshBuildInputs
            ++ [
              pkgs.just
              pkgs.clang-tools # clang-format for the devShell + editor LSP
              treefmtEval.config.build.wrapper
            ];
        };
      }
    );
}
