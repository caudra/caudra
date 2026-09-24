{
  description = "Caudra - terminal coding agent that turns context into effective action";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      nixpkgs,
      crane,
      rust-overlay,
      ...
    }:
    let
      lib = nixpkgs.lib;
      cargoToml = fromTOML (builtins.readFile ./Cargo.toml);
      packageName = cargoToml.package.name;
      version = cargoToml.workspace.package.version;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forEachSystem =
        f:
        lib.genAttrs systems (
          system:
          f system (
            import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            }
          )
        );

      mkCraneLib =
        pkgs:
        let
          rustToolchain = pkgs.rust-bin.stable."1.98.0".default.override {
            extensions = [
              "rust-src"
              "rust-analyzer"
            ];
          };
        in
        (crane.mkLib pkgs).overrideToolchain rustToolchain;

      mkWorkspaceSrc =
        craneLib:
        lib.cleanSourceWith {
          filter =
            path: type:
            (craneLib.filterCargoSources path type)
            || (builtins.match ".*/plugins/.*" path != null)
            || (builtins.match ".*/prompts/.*" path != null)
            || (builtins.match ".*/themes/.*" path != null)
            || (builtins.match ".*/words/.*" path != null)
            || (lib.hasSuffix ".lua" path);
          src = lib.cleanSource ./.;
        };

      cargoLockParsed = builtins.fromTOML (builtins.readFile ./Cargo.lock);

      # Exact Cargo.lock source strings (with fragment) of all git deps
      gitDepSources = lib.unique (
        builtins.filter (lib.hasPrefix "git+") (map (p: p.source or "") cargoLockParsed.package)
      );

      # Fixed-output fetches for git deps: cold evals skip full-history
      # builtins.fetchGit clones, and CI gets substitutable cache hits instead.
      # Keys embed the dep's tag/rev and locked commit, so a dep bump changes
      # the key itself: replace the old key with the new Cargo.lock source
      # string, set the hash to "", rebuild, and paste the hash from the error.
      # The `git-dep-hashes` CI check names both the key to add and the stale
      # key to remove.
      #
      # Two failure modes:
      # - Missing key (dep bumped, flake not yet updated): crane falls back
      #   to fetchGit with an eval warning. Nothing breaks; cold evals are
      #   just slower until the key and its hash are added.
      # - Wrong hash for an existing key: the build fails with a hash
      #   mismatch that prints the real hash. This is both the recovery
      #   path for updates and a hard stop if pinned content ever changes.
      gitDepHashes = {
        "git+https://github.com/pydantic/monty.git?tag=v0.0.21#70fe3f5781381eb33579e45046f8cb3845953373" =
          "sha256-P4PgqfYykkZrWGg5G3WQo070lORLEhmXQUQPx3+Yslo=";
        "git+https://github.com/crossterm-rs/crossterm?rev=3ca54292d2b1f1c58e200a06122ddaf5dd6b5c77#3ca54292d2b1f1c58e200a06122ddaf5dd6b5c77" =
          "sha256-A5lgiEEi7mktf7m2GljdAxst7Fdl7Uqko29Xq6o90Ow=";
        "git+https://github.com/tensorninja/workcell-mcp?rev=dceb80be5e039a86fb85363d05eb22eea5babbe4#dceb80be5e039a86fb85363d05eb22eea5babbe4" =
          "sha256-dUjsZ+6UyQRJUnW4KWxGzdUrc5/mFB9lfusHXnZoR6M=";
      };

      missingGitDepHashes = builtins.filter (s: !(builtins.hasAttr s gitDepHashes)) gitDepSources;

      staleGitDepHashes = builtins.filter (k: !(builtins.elem k gitDepSources)) (
        builtins.attrNames gitDepHashes
      );

      gitDepHashDrift =
        lib.optionalString (missingGitDepHashes != [ ]) ''
          missing entries (add with hash "", rebuild, paste the hash from the error):
            ${lib.concatStringsSep "\n  " missingGitDepHashes}
        ''
        + lib.optionalString (staleGitDepHashes != [ ]) ''
          stale entries (remove):
            ${lib.concatStringsSep "\n  " staleGitDepHashes}
        '';
    in
    {
      packages = forEachSystem (
        system: pkgs:
        let
          craneLib = mkCraneLib pkgs;
          workspaceSrc = mkWorkspaceSrc craneLib;
          montySrc = pkgs.fetchgit {
            url = "https://github.com/pydantic/monty.git";
            rev = "70fe3f5781381eb33579e45046f8cb3845953373";
            hash = "sha256-P4PgqfYykkZrWGg5G3WQo070lORLEhmXQUQPx3+Yslo=";
          };
          montyVendorDeps = craneLib.vendorCargoDeps { src = montySrc; };
          montyWorker = craneLib.buildPackage {
            pname = "caudra-monty-worker";
            version = "0.0.21";
            src = montySrc;
            cargoVendorDir = montyVendorDeps;
            cargoExtraArgs = "--package monty-runtime --no-default-features";
            doCheck = false;
            installPhaseCommand = ''
              mkdir -p $out/bin
              cp target/release/monty $out/bin/monty
              $out/bin/monty --version | grep -q '0.0.21'
            '';
          };

          # TODO: Upstream monty includes a relative README path that doesn't
          # survive nix vendoring. Remove this once `monty` stops including
          # the relative path
          vendorDeps = craneLib.vendorCargoDeps {
            src = workspaceSrc;
            outputHashes = gitDepHashes;
          };
          cargoVendorDir = pkgs.runCommandLocal "vendor-cargo-deps" { } ''
            cp -rL ${vendorDeps} $out
            chmod -R +w $out
            # config.toml has absolute paths to the original vendor dir;
            # rewrite them to point to our patched copy
            substituteInPlace "$out/config.toml" \
              --replace-fail "${vendorDeps}" "$out"
            find "$out" -name "*.rs" -print0 | while IFS= read -r -d "" f; do
              if grep -qF '#![doc = include_str!("../../../README.md")]' "$f"; then
                substituteInPlace "$f" \
                  --replace-fail '#![doc = include_str!("../../../README.md")]' \
                            '#![doc = "Monty Python bridge."]'
              fi
            done
          '';

          commonArgs = {
            nativeBuildInputs = with pkgs; [
              pkg-config
              perl
              python3
            ];
            buildInputs = with pkgs; [
              openssl
              stdenv.cc.cc.lib
            ];
            inherit cargoVendorDir;
            WORKCELL_BASH_EXECUTABLE = "${pkgs.bash}/bin/bash";
            WORKCELL_BUNDLED_MONTY_WORKER = "${montyWorker}/bin/monty";
          };

          cargoArtifacts = craneLib.buildDepsOnly (
            commonArgs
            // {
              pname = "${packageName}-deps";
              inherit version;
              src = workspaceSrc;
            }
          );
        in
        {
          default = craneLib.buildPackage (
            commonArgs
            // {
              pname = packageName;
              inherit version;
              src = workspaceSrc;
              cargoArtifacts = cargoArtifacts;
              cargoExtraArgs = "--package ${packageName}";
              doCheck = false;
              doInstallCheck = true;
              installCheckPhaseCommand = ''
                smoke_output="$(OPENAI_API_KEY=release-smoke \
                  XDG_CACHE_HOME="$TMPDIR/cache" \
                  $out/bin/caudra --model openai/gpt-5.1 prompt --tools --names 2>&1)"
                printf '%s\n' "$smoke_output" | grep -qx python_execution
                ! printf '%s\n' "$smoke_output" | grep -q 'Workcell python_execution is unavailable'
              '';
            }
          );
        }
      );

      checks = forEachSystem (
        system: pkgs: {
          git-dep-hashes =
            if missingGitDepHashes == [ ] && staleGitDepHashes == [ ] then
              pkgs.runCommandLocal "git-dep-hashes" { } "touch $out"
            else
              builtins.throw ''
                flake.nix gitDepHashes is out of sync with Cargo.lock git sources:
                ${gitDepHashDrift}'';
          fmt =
            pkgs.runCommandLocal "check-nix-format"
              {
                nativeBuildInputs = [ pkgs.nixfmt ];
                src = lib.cleanSource ./.;
              }
              ''
                find "$src" -name '*.nix' -type f -exec nixfmt --check {} +
                touch $out
              '';
        }
      );

      devShells = forEachSystem (
        _: pkgs:
        let
          craneLib = mkCraneLib pkgs;
          certs = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
        in
        {
          default = craneLib.devShell {
            packages = with pkgs; [
              cargo-machete
              cargo-nextest
              git
              just
              openssl
              perl
              pkg-config
              python3
              ripgrep
              ruff
              stylua
              ty
            ];

            SSL_CERT_FILE = certs;
            NIX_SSL_CERT_FILE = certs;

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
              pkgs.stdenv.cc.cc.lib
            ];
          };
        }
      );

      formatter = forEachSystem (_: pkgs: pkgs.nixfmt);
    };
}
