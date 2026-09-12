{
  description = "A Claude Code session transcript, rendered as streaming Markdown";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = ["x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin"];
    forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
  in {
    packages = forAll (pkgs: rec {
      claude-md-stream = pkgs.rustPlatform.buildRustPackage {
        pname = "claude-md-stream";
        version = "0.1.0";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;

        # Both binaries shell out to `herdr` by bare name, which has to be the
        # user's own build rather than one pinned here.
        meta = {
          description = "Render a Claude Code session as streaming Markdown";
          mainProgram = "claude-md-stream";
        };
      };
      default = claude-md-stream;
    });

    devShells = forAll (pkgs: {
      default = pkgs.mkShell {
        packages = [pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt];
      };
    });
  };
}
