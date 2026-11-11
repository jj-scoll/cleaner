{
  description = "cleaner: approve-first disk cleanup for Linux desktops";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs, ... }:
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
        nativeBuildInputs = with pkgs; [ pkg-config wrapGAppsHook4 copyDesktopItems ];
        buildInputs = with pkgs; [ gtk4 libadwaita ];
        # Units and the notification's GUI launch then run the wrapper, not
        # the bare binary behind it, so they keep its GTK environment.
        preFixup = ''
          gappsWrapperArgs+=(--set CLEANER_EXE "$out/bin/cleaner")
        '';
        desktopItems = [
          (pkgs.makeDesktopItem {
            name = "io.github.jj_scoll.Cleaner";
            desktopName = "Cleaner";
            comment = "Approve-first disk cleanup";
            exec = "cleaner gui";
            icon = "edit-clear-all-symbolic";
            categories = [ "System" "Utility" ];
          })
        ];
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

      # home-manager: `services.cleaner.enable = true;` installs the package and
      # declares the nightly plan and low-space timers, so they follow every
      # rebuild. `cleaner timer install` then refuses to touch them.
      homeManagerModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.cleaner;
          exe = "${cfg.package}/bin/cleaner";
          service = description: command: {
            Unit.Description = description;
            Service = {
              Type = "oneshot";
              ExecStart = "${exe} ${command}";
              Nice = 10;
              IOSchedulingClass = "idle";
            };
          };
        in
        {
          options.services.cleaner = {
            enable = lib.mkEnableOption "cleaner's nightly plan and low-space check";
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              defaultText = lib.literalExpression "cleaner.packages.\${system}.default";
              description = "The cleaner package to run.";
            };
            nightlyAt = lib.mkOption {
              type = lib.types.strMatching "([01][0-9]|2[0-3]):[0-5][0-9]";
              default = "03:00";
              description = "Daily time (HH:MM, 24-hour) of the nightly plan.";
            };
            lowSpaceEvery = lib.mkOption {
              type = lib.types.nullOr (lib.types.strMatching "[1-9][0-9]*(min|h)");
              default = "10min";
              description = "How often to check free space, e.g. 10min or 1h; null turns the check off.";
            };
          };

          config = lib.mkIf cfg.enable {
            home.packages = [ cfg.package ];
            systemd.user.services.cleaner = service "cleaner: build a cleanup plan (deletes nothing)" "plan --notify";
            systemd.user.timers.cleaner = {
              Unit.Description = "cleaner nightly plan";
              Timer = {
                OnCalendar = "*-*-* ${cfg.nightlyAt}:00";
                Persistent = true;
                RandomizedDelaySec = "5min";
              };
              Install.WantedBy = [ "timers.target" ];
            };
            systemd.user.services.cleaner-space = lib.mkIf (cfg.lowSpaceEvery != null)
              (service "cleaner: check free space (deletes nothing)" "plan --if-low");
            systemd.user.timers.cleaner-space = lib.mkIf (cfg.lowSpaceEvery != null) {
              Unit.Description = "cleaner low-space check";
              Timer = {
                OnStartupSec = "2min";
                OnUnitActiveSec = cfg.lowSpaceEvery;
                AccuracySec = "1min";
              };
              Install.WantedBy = [ "timers.target" ];
            };
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
