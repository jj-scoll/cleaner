{
  lib,
  rustPlatform,
  fetchFromGitHub,
  pkg-config,
  wrapGAppsHook4,
  copyDesktopItems,
  makeDesktopItem,
  gtk4,
  libadwaita,
  git,
  versionCheckHook,
  nix-update-script,
}:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "cleaner";
  version = "0.2.0";

  src = fetchFromGitHub {
    owner = "jj-scoll";
    repo = "cleaner";
    tag = "v${finalAttrs.version}";
    hash = "sha256-wzM8rYPUCRpKdk0Mu5VmlryJlViHqR+CQ0EXR/aRMDg=";
  };

  cargoHash = "sha256-gD7NIdl08/YlQAeGtblUzGmmAr0o2pHM+C6Z76/+p7k=";

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
    copyDesktopItems
  ];

  buildInputs = [
    gtk4
    libadwaita
  ];

  # The systemd units `cleaner timer install` writes, and the GUI launch from
  # the desktop notification, then run the wrapper rather than the unwrapped
  # binary behind it, so they keep its GTK environment.
  preFixup = ''
    gappsWrapperArgs+=(--set CLEANER_EXE "$out/bin/cleaner")
  '';

  postInstall = ''
    install -Dm644 assets/icon/cleaner.svg \
      $out/share/icons/hicolor/scalable/apps/io.github.jj_scoll.Cleaner.svg
  '';

  desktopItems = [
    (makeDesktopItem {
      name = "io.github.jj_scoll.Cleaner";
      desktopName = "Cleaner";
      comment = "Approve-first disk cleanup";
      exec = "cleaner gui";
      icon = "io.github.jj_scoll.Cleaner";
      categories = [
        "System"
        "Utility"
      ];
    })
  ];

  # The suite is hermetic (temp homes, faked docker/nix/uv/go/systemd/D-Bus)
  # but builds real git fixtures. The GTK smoke tests skip themselves when
  # there is no display, as in the build sandbox.
  nativeCheckInputs = [ git ];

  nativeInstallCheckInputs = [ versionCheckHook ];
  doInstallCheck = true;

  passthru.updateScript = nix-update-script { };

  meta = {
    description = "Approve-first disk cleanup for Linux desktops";
    longDescription = ''
      cleaner scans for regenerable junk (build artifacts, tool caches, stale
      git worktrees, Docker leftovers, old Nix generations) and writes a plan.
      It deletes only the entries you approve, re-validating each one first,
      and reports the space actually freed.
    '';
    homepage = "https://jj-scoll.github.io/cleaner/";
    changelog = "https://github.com/jj-scoll/cleaner/blob/v${finalAttrs.version}/CHANGELOG.md";
    license = lib.licenses.mit;
    # TODO: add yourself to maintainers/maintainer-list.nix in this PR, then:
    # maintainers = with lib.maintainers; [ <your-handle> ];
    maintainers = [ ];
    mainProgram = "cleaner";
    platforms = lib.platforms.linux;
  };
})
