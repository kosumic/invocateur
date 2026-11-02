{
  description = "Invocateur: summon coding agents in Herdr from GitHub comments";

  inputs.nixpkgs.url = "nixpkgs/nixos-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    forAllSystems = nixpkgs.lib.genAttrs [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ];
  in {
    packages = forAllSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      invocateur = pkgs.callPackage ./package.nix {};
      default = self.packages.${system}.invocateur;
    });

    homeManagerModules = {
      default = import ./nix/home-manager.nix;
      invocateur = self.homeManagerModules.default;
    };

    checks = forAllSystems (system: {
      invocateur = self.packages.${system}.invocateur;
    });

    formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.alejandra);

    devShells = forAllSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          rustc
          rustfmt
          clippy
          gcc
          gh
          git
          alejandra
        ];
      };
    });
  };
}
