{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "kaleidoscope-hostlink";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
      ./kaleidoscope-hostlink.service
    ];
  };

  cargoLock.lockFile = ./Cargo.lock;

  # Ship the systemd user unit inside the package, with the store path of the
  # binary filled in. NixOS picks it up via `systemd.packages`.
  postInstall = ''
    install -Dm444 kaleidoscope-hostlink.service \
      $out/lib/systemd/user/kaleidoscope-hostlink.service
    substituteInPlace $out/lib/systemd/user/kaleidoscope-hostlink.service \
      --replace-fail @out@ $out
  '';

  meta = {
    description = "Push desktop state to a Kaleidoscope keyboard for LED effects";
    platforms = lib.platforms.linux;
    mainProgram = "kaleidoscope-hostlink";
  };
}
