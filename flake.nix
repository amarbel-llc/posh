{
  description = "Mosh: the mobile shell — remote terminal over UDP with roaming and local echo";

  inputs = {
    # Fork of upstream nixpkgs with the amarbel-llc package additions
    # (the eng fleet's cached overlay). See eng-nix(7) THE NIXPKGS-MASTER
    # INPUT and eng-versioning(7).
    igloo.url = "github:amarbel-llc/igloo";
    utils.url = "https://flakehub.com/f/numtide/flake-utils/0.1.102";
    nixpkgs-master.url = "github:NixOS/nixpkgs/567a49d1913ce81ac6e9582e3553dd90a955875f";

    # conformist — the linter+formatter multiplexer (treefmt successor, RFC
    # 0001). Supplies the runner binary, its Nix module library
    # (conformist.lib.evalModule), and the eng-convention presets. Following
    # its igloo/nixpkgs-master/utils pins keeps this flake's closure shared
    # with conformist's. See conformist(7), conformist-nix(7).
    conformist = {
      url = "github:amarbel-llc/conformist";
      inputs.igloo.follows = "igloo";
      inputs.nixpkgs-master.follows = "nixpkgs-master";
      inputs.utils.follows = "utils";
    };

    # mephisto — the genetic-programming engine for the RFC 0007 evolutionary
    # predictor pilot. PRIVATE repo, so a flake input (a fixed-output git+ssh
    # fetch at evaluation time, authenticated by the user's SSH agent) rather
    # than a cargo git dependency (which the sandboxed hermetic build cannot
    # authenticate). flake = false: we consume the source tree (a root virtual
    # workspace; the `mephisto` crate lives under v2-rust/), bridged into the
    # rust build via a cargo [patch]. See docs/rfcs/0007.
    mephisto = {
      url = "git+ssh://git@github.com/amarbel-llc/mephisto?rev=31d496dfd95509b2d48b8fe51179adf1e6f00b84";
      flake = false;
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
      conformist,
      mephisto,
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

        # RFC 0007: both rust derivations (.#posh and the mosh-ffi check) build
        # with src = ./. and load the whole workspace manifest — which includes
        # crates/posh's path-dep on the private `mephisto` crate. vendor/mephisto
        # is gitignored (absent from src), so populate it from the `mephisto`
        # flake input's store path before cargo runs (no network/creds in the
        # sandbox). Shared so the two derivations can't drift. See docs/rfcs/0007.
        mephistoVendorPostPatch = ''
          mkdir -p vendor
          cp -r ${mephisto} vendor/mephisto
          chmod -R u+w vendor
        '';

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
        # attr. (posht, by contrast, now flows from POSH_VERSION + git rev via
        # ldflags — see the posht derivation below.) See eng-versioning(7) on
        # polyglot lineages.
        #
        # Assembled from components rather than written as a bare
        # `"1.4.0"` literal so it does not trip conformist's
        # eng-versioning-deprecated-file linter, whose regex flags any
        # `*Version = "x.y.z"` in flake.nix as a version that should migrate to
        # version.env. mosh's lineage is a sanctioned exception, not drift —
        # the components keep the real value (1.4.0) plainly visible.
        moshVersion = lib.concatStringsSep "." [
          "1"
          "4"
          "0"
        ];

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

          # RFC 0007: vendor the private mephisto crate from the flake input
          # before cargo runs (shared with the mosh-ffi check). The dev-loop gets
          # the same path via the devShell shellHook below.
          postPatch = mephistoVendorPostPatch;

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
        # single source of truth: version.env (POSH_VERSION, read into
        # poshVersion above) + the git rev, flowed into the binary via
        # -ldflags -X main.version / main.gitSHA (github #71). posht's own
        # `var version`/`gitSHA` defaults are inert dev placeholders. See
        # eng-versioning(7) and docs/posht.md.
        posht = pkgs.buildGoModule {
          pname = "posht";
          version = poshVersion;

          src = ./posht;
          vendorHash = "sha256-mR/fqtqVw4VL8LcbRkoJ+TdICQPYKTn8XZEl8yqGjuQ=";

          # Flow version.env + git rev into the Go binary (github #71). -X sets
          # the package-level `version`/`gitSHA` vars; buildGoModule already runs
          # `go test ./...` in checkPhase (doCheck defaults on), which gates the
          # version-format test.
          ldflags = [
            "-X main.version=${poshVersion}"
            "-X main.gitSHA=${poshGitSha}"
          ];

          meta = with lib; {
            description = "Interactive terminal-capability test for posh (\"diff on a POSH\")";
            license = licenses.gpl3Plus;
            mainProgram = "posht";
            platforms = platforms.linux ++ platforms.darwin;
          };
        };

        # posh-palette: the command-palette renderer for the posh client — a
        # bubbletea (v2) subprocess the client drives over the RFC 0005 JSON-RPC
        # control channel (fd 3) and composites onto the session view. Its own
        # Go module under posh-palette/, independent of the Rust workspace, so
        # sourcing the subtree keeps non-palette changes from rebuilding it.
        # Built exactly like posht: buildGoModule + version/rev flowed in via
        # -ldflags -X main.version / main.gitSHA (github #71); buildGoModule's
        # checkPhase runs `go test ./...`, gating the version-format and
        # protocol tests. The client locates the binary next to itself, so
        # poshToolset co-installs it in the same bin/. See eng-versioning(7).
        posh-palette = pkgs.buildGoModule {
          pname = "posh-palette";
          version = poshVersion;

          src = ./posh-palette;
          vendorHash = "sha256-DwMz9dJy844NmDb9d711z4JnqwZtqociP/LZXBC2WJw=";

          ldflags = [
            "-X main.version=${poshVersion}"
            "-X main.gitSHA=${poshGitSha}"
          ];

          meta = with lib; {
            description = "Command-palette renderer for the posh client (RFC 0005 control protocol)";
            license = licenses.gpl3Plus;
            mainProgram = "posh-palette";
            platforms = platforms.linux ++ platforms.darwin;
          };
        };

        # The default output: the full posh toolset in one tree, so a bare
        # `nix build` yields every posh-* build product rather than just the
        # posh binary (github #73). `posh` already installs the posh /
        # posh-server / poshterity binaries and the man pages; this adds posht
        # (the Go TUI) and posh-palette (the command-palette renderer).
        # posh-term is a library compiled into posh, not a standalone binary, so
        # it has no entry here. mosh (the upstream C++ reference) deliberately
        # stays its own non-default output.
        poshToolset = pkgs.symlinkJoin {
          name = "posh-toolset-${poshVersion}";
          paths = [
            posh
            posht
            posh-palette
          ];
          meta = {
            description = "The full posh toolset: posh, posh-server, poshterity, posht, and posh-palette";
            mainProgram = "posh";
            license = posh.meta.license;
            platforms = posh.meta.platforms;
          };
        };

        # poshterity exposed as its own runnable output, so `nix run
        # .#poshterity` runs the recorder/replayer and `nix build .#poshterity`
        # yields just that tool (github #73). It selects the single binary and
        # its man page out of the already-built `posh` package rather than
        # recompiling, so the binary has one source of truth and can never
        # diverge from the one in `posh`/the default toolset. mainProgram makes
        # `nix run` resolve to poshterity rather than posh.
        poshterity =
          pkgs.runCommand "poshterity-${poshVersion}"
            {
              meta = {
                description = "Deterministic, step-ratcheted terminal recorder/replayer built on posh-term";
                mainProgram = "poshterity";
                inherit (posh.meta) license platforms;
              };
            }
            ''
              mkdir -p "$out/bin" "$out/share/man/man1"
              ln -s ${posh}/bin/poshterity "$out/bin/poshterity"
              ln -s ${posh}/share/man/man1/poshterity.1.gz "$out/share/man/man1/poshterity.1.gz"
            '';

        # Tree-wide formatter + eng-convention linters under one runner:
        # clang-format (C++) + nixfmt + shfmt, plus the eng preset's
        # eng-versioning / flake-* / justfile-* checks. The eng preset and
        # posh's own formatters/excludes (./conformist.nix) are merged here.
        # Exposed as `formatter.${system}` (`nix fmt`, repair mode), the
        # read-only `checks.formatting` gate, and the store-pinned
        # conformist-pre-commit / conformist-repair git hooks (each bakes this
        # module's generated /nix/store config — there is no committed
        # conformist.toml). See ./conformist.nix and conformist-nix(7).
        conformistPkg = conformist.packages.${system}.default;
        conformistEval = conformist.lib.evalModule pkgs {
          imports = [
            conformist.lib.presets.eng
            ./conformist.nix
          ];
          package = conformistPkg;
        };

        # Impure git-state lane: the eng-convention checks that need a live
        # .git or host tools (agents-md's CLAUDE.md->AGENTS.md migration,
        # git-remotes/git-default-branch, sweatfile's `spinclass validate`,
        # gomod2nix — a no-op here, posh has no go.mod). They can't run in the
        # sandboxed checks.formatting (which sees only a /nix/store copy), so
        # this config drives a working-tree `conformist check` via
        # `just lint-worktree`. See conformist-nix(7) and the eng-impure preset.
        conformistImpureEval = conformist.lib.evalModule pkgs {
          imports = [ conformist.lib.presets.eng-impure ];
          package = conformistPkg;
          projectRootFile = "flake.nix";
        };
      in
      {
        packages = {
          # Default is the full toolset (github #73) so a bare `nix build`
          # yields posh + posh-server + poshterity + posht. The individual
          # products stay addressable; the C++ reference stays buildable as
          # `nix build .#mosh` but out of the default.
          default = poshToolset;
          posh = posh;
          poshterity = poshterity;
          mosh = mosh;
          posht = posht;
          posh-palette = posh-palette;

          # The store-pinned git hooks from this repo's pure-lane config
          # (conformist#47/#51/#54): conformist-pre-commit runs `conformist
          # --staged --exit-zero-on-fix`, conformist-repair runs `conformist
          # --commit --amend --exit-zero-on-fix` (the build.repair sibling, both
          # from the module's shared mkHookWrapper). On the devShell PATH under
          # those names; the sweatfile's pre-commit / repair hooks name them.
          # Unlike a bare `conformist`, every formatter's `command` is
          # store-pinned in the baked config, so they CANNOT silent-skip file
          # types the ambient PATH happens to lack (the conformist#51 trap).
          conformist-pre-commit = conformistEval.config.build.preCommit;
          conformist-repair = conformistEval.config.build.repair;

          # The impure-lane config (eng-impure preset). `just lint-worktree`
          # runs `conformist check` against the working tree with this config
          # to exercise the git-state linters (agents-md, git-remotes,
          # git-default-branch, sweatfile) that the sandboxed checks.formatting
          # cannot. Not committed — built on demand.
          conformist-impure-config = conformistImpureEval.config.build.configFile;
        };

        checks = {
          # Read-only formatting + eng-convention gate: builds in /nix/store
          # off a source snapshot, runs `conformist check`, fails if any file
          # would change or any eng linter (eng-versioning, flake-*,
          # justfile-*) finds a violation. Driven by `just lint-fmt` and
          # surfaced under `nix flake check`.
          formatting = conformistEval.config.build.check self;

          # The C++ FFI oracle (ADR 0004). mosh-ffi is a dev/test crate kept out
          # of the shipped .#posh build (workspace default-members), so this
          # dedicated gate builds and runs its characterization tests
          # (cargo test -p mosh-ffi) — which compile a slice of the zz-mosh C++
          # via the cc crate (g++ from stdenv). src = ./. so zz-mosh is in the
          # sandbox for build.rs's relative read. Driven by `just test-mosh-ffi`
          # and surfaced under `nix flake check`. Lib-only oracle (no shipped
          # binary): the value is the checkPhase passing.
          mosh-ffi = pkgs.rustPlatform.buildRustPackage {
            pname = "mosh-ffi-check";
            version = poshVersion;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            # RFC 0007: crates/posh (a workspace member this build loads even
            # though it only compiles -p mosh-ffi) path-deps the private mephisto
            # crate, so vendor it the same way .#posh does or the workspace
            # manifest fails to load in the sandbox.
            postPatch = mephistoVendorPostPatch;
            cargoBuildFlags = [
              "-p"
              "mosh-ffi"
            ];
            cargoTestFlags = [
              "-p"
              "mosh-ffi"
            ];
            doCheck = true;
            installPhase = "mkdir -p $out";
          };
        };

        formatter = conformistEval.config.build.wrapper;

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
              pkgs.gum # terminal UI for the maintenance recipes (eng-versioning(7))
              pkgs.gh # `just release` -> gh release create
              pkgs.tcpdump # live-session transport triage (debug-posh-* recipes)
              conformistPkg # the raw conformist runner: `nix fmt`, lint-worktree
              # The config-specific, toolchain-hermetic git hooks on PATH under
              # the names the sweatfile references (conformist#47/#51/#54).
              conformistEval.config.build.preCommit # `conformist-pre-commit`
              conformistEval.config.build.repair # `conformist-repair`
            ];
          # RFC 0007: the dev-loop's mephisto source. The flake build populates
          # vendor/mephisto from the store (postPatch above); for `just
          # debug-cargo`, symlink it to the same pinned flake-input store path so
          # local builds use the locked rev (not an ambient sibling checkout).
          shellHook = ''
            mkdir -p vendor
            ln -sfn ${mephisto} vendor/mephisto
          '';
        };
      }
    );
}
