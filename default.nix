{ lib, rustPlatform, makeWrapper, pulseaudio }:

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

  nativeBuildInputs = [ makeWrapper ];

  # The daemon runs `pactl` (talking to PipeWire's PulseAudio compatibility
  # server) to watch the microphone's mute state, so put it on the daemon's PATH.
  # Then ship the systemd user unit inside the package, with the store path of
  # the binary filled in. NixOS picks it up via `systemd.packages`.
  postInstall = ''
    wrapProgram $out/bin/kaleidoscope-hostlink \
      --prefix PATH : ${lib.makeBinPath [ pulseaudio ]}
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
