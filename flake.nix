{
  description = "cleaner: approve-first disk cleanup for Linux desktops";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in
    {
      # `nix run github:jj-scoll/cleaner`: the full build, GUI included.
      packages.x86_64-linux.default = pkgs.rustPlatform.buildRustPackage {
        pname = "cleaner";
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./tests ];
        };
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = with pkgs; [ pkg-config wrapGAppsHook4 ];
        buildInputs = with pkgs; [ gtk4 libadwaita ];
        # CI runs the suite; it needs git and a display, which the sandbox lacks.
        doCheck = false;
        meta = {
          description = "Approve-first disk cleanup for Linux desktops";
          homepage = "https://jj-scoll.github.io/cleaner/";
          license = pkgs.lib.licenses.mit;
          mainProgram = "cleaner";
          platforms = pkgs.lib.platforms.linux;
        };
      };

      # The system cargo/rustc are used; mkShell puts stdenv.cc first on PATH,
      # so its ld-wrapper adds an rpath for the GTK libraries.
      devShells.x86_64-linux.default = pkgs.mkShell {
        packages = with pkgs; [ pkg-config gtk4 libadwaita ];
      };

      # CI: the toolchain comes from nixpkgs too, plus Xvfb for the GUI tests.
      devShells.x86_64-linux.ci = pkgs.mkShell {
        packages = with pkgs; [ pkg-config gtk4 libadwaita cargo rustc clippy xvfb-run git ];
      };
    };
}
