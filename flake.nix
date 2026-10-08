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
          rustToolchain = pkgs.rust-bin.stable."1.99.0".default.override {
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
            || (builtins.match ".*/vendor/crossterm/.*" path != null)
            || (builtins.match ".*/plugins/.*" path != null)
            || (builtins.match ".*/prompts/.*" path != null)
            || (builtins.match ".*/themes/.*" path != null)
            || (builtins.match ".*/words/.*" path != null)
            || (builtins.match ".*/decisions/questions/.*" path != null)
            || (builtins.match ".*/caudra-workflow/(builtins|skill)/.*" path != null)
            || (builtins.match ".*/caudra-automation/(skill|tests/examples)/.*" path != null)
            || (builtins.match ".*/docs/(content|examples)/.*" path != null)
            || (lib.hasSuffix "/docs/navigation.json" path)
            || (builtins.match ".*/workcell/.*/(queries|rules.*|fixtures|evals)/.*" path != null)
            || (builtins.match ".*/workcell/fixtures/.*" path != null)
            || (builtins.match ".*/scripts/.*" path != null)
            || (builtins.match ".*/THIRD_PARTY_LICENSES/.*" path != null)
            || (builtins.match ".*/(LICENSE[^/]*|COPYING[^/]*|NOTICE[^/]*|THIRD_PARTY[^/]*)" path != null)
            || (lib.hasSuffix ".lua" path);
          src = lib.cleanSource ./.;
        };

      mkWorkspaceDummySrc =
        craneLib:
        craneLib.mkDummySrc {
          src = mkWorkspaceSrc craneLib;
          extraDummyScript = ''
            rm -rf "$out/vendor/crossterm"
            cp -r ${./vendor/crossterm} "$out/vendor/crossterm"
          '';
        };

      materializeVendor = vendor: ''
        cp -rL ${vendor} "$out"
        chmod -R u+w "$out"
        substituteInPlace "$out/config.toml" \
          --replace-fail "${vendor}" "$out"
      '';

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
        "git+https://github.com/pydantic/monty.git?tag=v1.0.0#85c5d1f6bef038405cfc40a4eed94806e303567e" =
          "sha256-tuDFwYLIprdVyAH47rqYiI4xU3RCuNdkBjIyVM1JeWE=";
        "git+https://github.com/modelcontextprotocol/rust-sdk.git?rev=9334c97f0d6e177546fc33593148aa3f942cb1f8#9334c97f0d6e177546fc33593148aa3f942cb1f8" =
          "sha256-X9WsE8CB5rBwXClvZibtKA6ajoYuAOIA3mj4ETN9gyQ=";
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
            rev = "85c5d1f6bef038405cfc40a4eed94806e303567e";
            hash = "sha256-tuDFwYLIprdVyAH47rqYiI4xU3RCuNdkBjIyVM1JeWE=";
          };
          montyVendorDeps = craneLib.vendorCargoDeps { src = montySrc; };
          montyAttributionVendor = pkgs.runCommandLocal "monty-attribution-vendor" { } (
            materializeVendor montyVendorDeps
          );
          montyWorker = craneLib.buildPackage {
            pname = "caudra-monty-worker";
            version = "1.0.0";
            src = montySrc;
            cargoVendorDir = montyVendorDeps;
            cargoExtraArgs = "--package monty-runtime --no-default-features";
            CARGO_PROFILE_RELEASE_LTO = "thin";
            CARGO_PROFILE_RELEASE_CODEGEN_UNITS = "1";
            CARGO_PROFILE_RELEASE_STRIP = "none";
            outputs = [
              "out"
              "debug"
            ];
            doCheck = false;
            installPhaseCommand = ''
              mkdir -p $out/bin $debug/bin
              cp target/release/monty $debug/bin/monty
              cp target/release/monty $out/bin/monty
              $STRIP $out/bin/monty
              $out/bin/monty --version | grep -qx 'monty-runtime 1.0.0'
            '';
            dontStrip = true;
          };

          # TODO: Upstream monty includes a relative README path that doesn't
          # survive nix vendoring. Remove this once `monty` stops including
          # the relative path
          vendorDeps = craneLib.vendorCargoDeps {
            src = workspaceSrc;
            outputHashes = gitDepHashes;
            overrideVendorGitCheckout =
              _: drv:
              drv.overrideAttrs (previous: {
                postInstall = (previous.postInstall or "") + ''
                  cargo metadata --offline --format-version 1 --no-deps |
                    jq -r '.packages[] | [.manifest_path, (.name + "-" + .version)] | @tsv' |
                    while IFS=$'\t' read -r manifest package; do
                      if [ -f "$out/$package/Cargo.toml" ]; then
                        cmp <(crane-resolve-workspace-inheritance "$manifest") "$out/$package/Cargo.toml" || exit 1
                        cp "$manifest" "$out/$package/Cargo.toml.orig"
                      fi
                    done
                '';
              });
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
            CARGO_PROFILE_RELEASE_STRIP = "none";
          };

          cargoArtifacts = craneLib.buildDepsOnly (
            commonArgs
            // {
              pname = "${packageName}-deps";
              inherit version;
              dummySrc = mkWorkspaceDummySrc craneLib;
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
              outputs = [
                "out"
                "debug"
              ];
              postBuild = ''
                worker_source="$(mktemp -d)"
                cp -r ${montySrc}/. "$worker_source/"
                chmod -R u+w "$worker_source"
                mkdir -p "$worker_source/.cargo"
                cp ${montyAttributionVendor}/config.toml "$worker_source/.cargo/config.toml"
                CARGO_NET_OFFLINE=true python3 scripts/build-attribution.py \
                  --manifest-path Cargo.toml --package ${packageName} \
                  --target ${pkgs.stdenv.hostPlatform.rust.rustcTarget} \
                  --worker-manifest-path "$worker_source/Cargo.toml" \
                  --worker-package monty-runtime --output-dir licenses
                test -s licenses/manifest.json
              '';
              installPhaseCommand = ''
                mkdir -p $out/bin $debug/bin
                mkdir -p $out/share/licenses $debug/share/licenses
                cp -r licenses $out/share/licenses/caudra
                ln -s $out/share/licenses/caudra $debug/share/licenses/caudra
                cp target/release/caudra $debug/bin/caudra
                ln -s ${montyWorker.debug}/bin/monty $debug/bin/monty
                cp target/release/caudra $out/bin/caudra
                $STRIP $out/bin/caudra
              '';
              dontStrip = true;
              doCheck = false;
              doInstallCheck = true;
              installCheckPhase = ''
                runHook preInstallCheck
                HOME="$(mktemp -d)"
                export HOME
                export XDG_CONFIG_HOME="$HOME/config"
                export XDG_DATA_HOME="$HOME/data"
                export XDG_STATE_HOME="$HOME/state"
                export XDG_CACHE_HOME="$HOME/cache"
                export XDG_RUNTIME_DIR="$HOME/runtime"
                mkdir -m 700 "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME" \
                  "$XDG_CACHE_HOME" "$XDG_RUNTIME_DIR"
                if ! smoke_output="$(OPENAI_API_KEY=release-smoke \
                  "$out/bin/caudra" --model openai/gpt-5.1 tools --enabled-only --names 2>&1)"; then
                  printf '%s\n' "$smoke_output" >&2
                  exit 1
                fi
                if ! printf '%s\n' "$smoke_output" | grep -qx python_execution ||
                  printf '%s\n' "$smoke_output" | grep -q 'Workcell python_execution is unavailable'; then
                  printf '%s\n' "$smoke_output" >&2
                  exit 1
                fi
                runHook postInstallCheck
              '';
            }
          );
        }
      );

      checks = forEachSystem (
        system: pkgs: {
          vendor-attribution =
            let
              vendor = pkgs.runCommandLocal "attribution-vendor-fixture" { } ''
                mkdir -p "$out/package/src" "$out/package/ancillary" "$out/sources"
                printf '[package]\nname = "dependency"\nversion = "1.0.0"\n' > "$out/package/Cargo.toml"
                printf 'pub fn dependency() {}\n' > "$out/package/src/lib.rs"
                printf 'Copyright fixture authors\n' > "$out/package/ancillary/notice.txt"
                printf '\000\377ancillary source\n' > "$out/package/ancillary/payload.dat"
                ln -s "$out/package/ancillary/notice.txt" "$out/package/NOTICE"
                ln -s "$out/package" "$out/sources/dependency-1.0.0"
                ln -s "$out/sources" "$out/registry"
                printf '[source.fixture]\ndirectory = "%s/registry"\n' "$out" > "$out/config.toml"
              '';
              materialized = pkgs.runCommandLocal "attribution-vendor-materialized" { } (
                materializeVendor vendor
              );
            in
            pkgs.runCommandLocal "check-vendor-attribution" { nativeBuildInputs = [ pkgs.python3 ]; } ''
              python3 - <<'PY'
              import importlib.util
              from pathlib import Path
              import tarfile
              import tomllib

              spec = importlib.util.spec_from_file_location("attribution", "${./scripts/build-attribution.py}")
              attribution = importlib.util.module_from_spec(spec)
              spec.loader.exec_module(attribution)
              original = Path("${vendor}")
              vendor = Path("${materialized}")
              config = (vendor / "config.toml").read_text()
              registry = Path(tomllib.loads(config)["source"]["fixture"]["directory"])
              assert registry == vendor / "registry"
              assert str(original) not in config
              attribution.files(vendor)
              crate = registry / "dependency-1.0.0"
              assert {p.relative_to(crate).as_posix() for p in attribution.license_files(crate)} == {
                  "NOTICE", "ancillary/notice.txt"
              }
              archive = Path("source.tar.gz")
              attribution.source_archive(crate, archive)
              with tarfile.open(archive) as source:
                  assert set(source.getnames()) == {
                      "Cargo.toml", "NOTICE", "src/lib.rs", "ancillary/notice.txt", "ancillary/payload.dat"
                  }
                  for member in source:
                      assert source.extractfile(member).read() == (original / "package" / member.name).read_bytes()
              PY
              touch $out
            '';
          dummy-src =
            let
              dummySrc = mkWorkspaceDummySrc (mkCraneLib pkgs);
            in
            pkgs.runCommandLocal "check-dummy-src" { } ''
              diff -r ${./vendor/crossterm} ${dummySrc}/vendor/crossterm
              touch $out
            '';
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
                find "$src" -type d -name fixtures -prune -o \
                  -name '*.nix' -type f -exec nixfmt --check {} +
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
              bash
              cargo-machete
              cargo-nextest
              git
              gnumake
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
