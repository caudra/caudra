# Development shell and package set for the workcell tooling.
{ pkgs ? import <nixpkgs> { }, lib ? pkgs.lib, system ? builtins.currentSystem }:

let
  version = "0.4.1";

  buildTool = { name, src }:
    pkgs.stdenv.mkDerivation {
      pname = name;
      inherit version src;
      nativeBuildInputs = [ pkgs.cargo pkgs.rustc ];
    };
in
rec {
  inherit version system;
  inherit (pkgs) stdenv;

  meta = {
    description = "Workcell execution server";
    license = lib.licenses.mit;
    platforms = lib.platforms.unix;
  };

  packages.workcell = buildTool {
    name = "workcell";
    src = ./.;
  };

  devShell = pkgs.mkShell {
    packages = [ pkgs.cargo pkgs.clippy pkgs.rustfmt ];
    shellHook = ''
      export RUST_LOG=info
    '';
  };
}
