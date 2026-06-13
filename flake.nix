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

        # posh's single source of truth: version.env at the repo root
        # (POSH_VERSION). Read here for the derivation `version` attr and
        # passed into the build env so each crate's build.rs flows it into
        # the crate (cargo:rustc-env=POSH_VERSION) without relying on the
        # relative-path read. The Cargo manifests' package.version is an
        # inert "0.0.0" placeholder that build.rs overrides at compile time,
        # so there is nothing to keep in lockstep and no drift. See
        # eng-versioning(7).
        poshVersion = builtins.head (
          builtins.match ".*POSH_VERSION=([^\n]+).*" (builtins.readFile ./version.env)
        );

        # Git revision embedded into `posh version` (github #63). The flake
        # exposes shortRev on a clean tree and dirtyShortRev (a "<sha>-dirty"
        # string) on a modified one; "unknown" when built from a non-git source.
        # Passed into the build env so crates/posh/build.rs flows it into the
        # binary (cargo:rustc-env=POSH_GIT_SHA). See eng-versioning(7).
        poshGitSha = self.shortRev or self.dirtyShortRev or "unknown";

        # Independent lineage: the vendored C++ mosh reference tracks
        # UPSTREAM mosh (AC_INIT([mosh],[1.4.0]) in configure.ac), not an
        # eng-released artifact, so it keeps its own literal rather than
        # POSH_VERSION. The autotools build derives its own VERSION.stamp
        # from git-describe (absent in the nix sandbox, so it falls back to
        # "mosh 1.4.0"); this literal is only for the derivation `version`
        # attr. (posht likewise keeps its Go-literal version.) See
        # eng-versioning(7) on polyglot lineages.
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

          # The C++ reference tree lives under zz-mosh/ (top level is the
          # posh Rust workspace); sourcing the subtree also keeps Rust-only
          # changes from rebuilding the C++ derivation.
          src = ./zz-mosh;

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

        # The Rust workspace: the posh rewrite (crates/posh-term +
        # crates/posh). `cargo test --workspace` runs in the sandboxed
        # checkPhase, making this the hermetic Rust CI gate (github #33).
        # The e2e tests drive ptys and loopback UDP — both available in the
        # Linux sandbox (the same facilities the C++ --local tests use).
        # Version single source of truth: version.env (POSH_VERSION),
        # read into poshVersion above. crates/posh/build.rs guards
        # Cargo.toml's package.version against it.
        posh = pkgs.rustPlatform.buildRustPackage {
          pname = "posh";
          version = poshVersion;

          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # Each crate's build.rs reads POSH_VERSION from the build env and
          # flows it into the crate (cargo:rustc-env=POSH_VERSION); runtime
          # reads env!("POSH_VERSION"). The nix `src` also carries version.env
          # into the sandbox, but the env var makes the build independent of
          # the relative-path read. See eng-versioning(7).
          POSH_VERSION = poshVersion;
          # Git rev for `posh version` (#63); the sandbox has no .git, so the
          # flake supplies it and build.rs flows it in.
          POSH_GIT_SHA = poshGitSha;

          # scdoc compiles the hand-written man pages in doc/*.scd during
          # postInstall. See eng-manpages(7) and doc/.
          nativeBuildInputs = [ pkgs.scdoc ];

          doCheck = true;

          # mosh-server parity: the ssh bootstrap runs `posh-server new` on
          # the remote host, so the package provides that name too (same
          # binary; `posh-server ...` == `posh server ...` via argv[0]).
          #
          # Man pages: compile every doc/*.scd into the matching man section,
          # deriving the section from the filename suffix (posh.1.scd ->
          # man1/posh.1). They land in this binary's own $out, so a consumer
          # home-manager `programs.man` surfaces them with no extra wiring.
          # See eng-manpages(7).
          postInstall = ''
            ln -s posh $out/bin/posh-server
            for f in doc/*.scd; do
              stem="$(basename "$f" .scd)"          # e.g. posh.1
              section="''${stem##*.}"               # 1
              name="''${stem%.*}"                   # posh
              mkdir -p "$out/share/man/man''${section}"
              scdoc < "$f" > "$out/share/man/man''${section}/''${name}.''${section}"
            done
          '';

          meta = with lib; {
            description = "Persistent, roaming terminal sessions: a combined Rust rewrite of zmx and mosh";
            license = licenses.gpl3Plus;
            mainProgram = "posh";
            platforms = platforms.linux ++ platforms.darwin;
          };
        };

        # posht ("diff on a POSH"): the standalone interactive terminal-
        # capability test (Go / Bubble Tea). Its own Go module under posht/,
        # independent of the Rust workspace and the C++ tree, so sourcing the
        # subtree keeps non-posht changes from rebuilding it. Pure Go, so the
        # binary is static and needs nothing on a target beyond a UTF-8 locale
        # — which is what lets `run-remote.sh` scp it to any host. Version
        # single source of truth: the `version` const in posht/main.go (a Go
        # literal, distinct from the Cargo/mosh versions). See docs/posht.md.
        posht = pkgs.buildGoModule {
          pname = "posht";
          version = "0.1.0"; # matches `const version` in posht/main.go

          src = ./posht;
          vendorHash = "sha256-mR/fqtqVw4VL8LcbRkoJ+TdICQPYKTn8XZEl8yqGjuQ=";

          meta = with lib; {
            description = "Interactive terminal-capability test for posh (\"diff on a POSH\")";
            license = licenses.gpl3Plus;
            mainProgram = "posht";
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
          # posh is the product; the C++ reference tree stays buildable as
          # `nix build .#mosh`.
          default = posh;
          posh = posh;
          mosh = mosh;
          posht = posht;
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
              pkgs.cargo # Rust workspace dev-loop (just debug-cargo)
              pkgs.rustc
              pkgs.scdoc # compile/lint doc/*.scd man pages (just lint-doc)
              treefmtEval.config.build.wrapper
            ];
        };
      }
    );
}
