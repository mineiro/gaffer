{ lib, rustPlatform, src }:

rustPlatform.buildRustPackage {
  pname = "gaffer";
  version = "0.1.0";
  inherit src;

  # The lockfile is committed, so Nix reads it directly instead of needing a
  # cargoHash that has to be re-guessed on every dependency bump.
  cargoLock.lockFile = ../Cargo.lock;

  # Nothing to link against: no GUI toolkit, no Avahi, no TLS stack. The
  # absence of buildInputs here is a property worth preserving.
  buildInputs = [ ];
  nativeBuildInputs = [ ];

  # `cargo test` runs in the sandbox, which has no network and no D-Bus. Every
  # test in this repo is hermetic by design, so that is exactly the right
  # environment for them — if one ever needs a bus, it belongs in the NixOS VM
  # test instead.
  doCheck = true;

  postInstall = ''
    install -Dm644 data/gaffer.service.in \
      $out/lib/systemd/user/gaffer.service
    install -Dm644 data/io.mineiro.gaffer.service.in \
      $out/share/dbus-1/services/io.mineiro.gaffer.service

    # @BINDIR@ becomes the store path. --replace-fail rather than --replace so
    # a renamed placeholder breaks the build instead of silently shipping a
    # unit that points nowhere.
    substituteInPlace \
      $out/lib/systemd/user/gaffer.service \
      $out/share/dbus-1/services/io.mineiro.gaffer.service \
      --replace-fail '@BINDIR@' "$out/bin"
  '';

  meta = {
    description = "Elgato Key Light control for Linux: a D-Bus session daemon plus a CLI";
    homepage = "https://github.com/mineiro/gaffer";
    license = lib.licenses.gpl3Plus;
    mainProgram = "gaffer";
    platforms = lib.platforms.linux;
  };
}
