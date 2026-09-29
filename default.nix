{ lib, rustPlatform }:

rustPlatform.buildRustPackage (finalAttrs: {
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

  passthru.services.default = {
    _class = "service";

    process.argv = [
      "${finalAttrs.finalPackage}/bin/kaleidoscope-hostlink"
    ];

    systemd.services."".serviceConfig = {
      Restart = "on-failure";
      RestartSec = 2;
    };

    systemd.services."".unitConfig = {
      Description =
        "Push desktop state to a Kaleidoscope keyboard for LED effects";
    };
  };

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
})
