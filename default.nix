# Nix derivation to build a release version of journaldriver.
#
# Note: This does not currently use Carnix due to an issue with
# linking against the `systemd.dev` derivation for libsystemd.

{ pkgs ? import <nixpkgs> {}
, doCheck ? true }:

with pkgs; rustPlatform.buildRustPackage {
  inherit doCheck;

  name        = "journaldriver";
  version     = "1.0.0";
  cargoSha256 = "03rq96hzv97wh2gbzi8sz796bqgh6pbpvdn0zy6zgq2f2sgkavsl";

  src = ./.;

  buildInputs = [ pkgconfig openssl systemd.dev ];
}
