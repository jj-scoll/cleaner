{
  description = "cleaner: approve-first disk cleanup (dev shell)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in
    {
      # The system cargo/rustc are used; mkShell puts stdenv.cc first on PATH,
      # so its ld-wrapper adds an rpath for the GTK libraries.
      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = with pkgs; [ pkg-config gtk4 libadwaita ];
      };
    };
}
