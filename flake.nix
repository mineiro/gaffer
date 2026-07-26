{
  description = "Elgato Key Light control for Linux: a D-Bus session daemon plus a CLI";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        gaffer = pkgs.callPackage ./nix/package.nix { src = self; };
        default = gaffer;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            # For poking at the running daemon: `busctl`, `avahi-browse`.
            systemd
            avahi
          ];
        };
      });

      nixosModules = {
        gaffer = import ./nix/module.nix;
        default = self.nixosModules.gaffer;
      };

      # `nix flake check` builds the package on every system and boots a NixOS
      # guest to prove the module actually works. See nix/test.nix for why that
      # needs a VM rather than another sandboxed build.
      checks = forAllSystems (pkgs: {
        package = self.packages.${pkgs.system}.gaffer;
      }
      // nixpkgs.lib.optionalAttrs (pkgs.system == "x86_64-linux") {
        activation = import ./nix/test.nix { inherit pkgs self; };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
