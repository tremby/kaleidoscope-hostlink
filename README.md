Kaleidoscope-hostlink
=====================

This is a daemon which runs on the host computer,
and a plugin for Kaleidoscope.

The daemon sends desktop state information to the keyboard,
so that it can be used for LED effects.

Vibecode warning
----------------

This was vibecoded with Claude.

Features
--------

### Hyprland workspaces integration

The daemon watches hyprland to see the state of the workspaces.
Additionally, it tracks whether any given workspace has an urgent client,
since Hyprland doesn't provide this information.

Then, when the super key is held down,
numbers 1 through 0 light up depending on the state of workspaces 1 through 10,
and the tab key lights up depending on the state of a special workspace.

From most priority to least:

- Urgent workspace: flash red (and on the off-flash, pass through...)
- Active workspace: white
- Workspace with a fullscreen window: rainbow
- Visible workspace: yellow-green
- Workspace with clients: dark green
- Other workspace: dark blue

### OBS integration

*This is very specific to my particular setup.
Work could potentially be done in future to make this configurable.*

The daemon connects to the OBS websocket server and also watches Pulseaudio.

When it is able to connect to OBS, some lighting is enabled.
The idea is to remind the user which keys are hotkeys for which scenes,
and to warn when things are in non-standard states.

- The 1 key is assumed to activate a scene called "Main scene",
  and is magenta.
  It is bright when that scene is live, and dimmer otherwise.
- The 2 key is assumed to activate a scene called "Camera scene", and is green.
  It flashes white while that scene is live.
- The 3 key is assumed to activate a scene called "Notes scene", and is dim yellow.
  It flashes white while that scene is live.
- The 4 key is assumed to toggle layers called "Transparent camera" which is usually visible.
  It is green, and flashes white while a scene containing a "Transparent camera" item is live but the item is hidden.
- The 5 key is assumed to toggle layers called "Digital notes overlay (game)" and "Digital notes overlay (notes)", which are usually hidden.
  It is dim yellow, and flashes white when one of those layers is visible on a live scene.
- The M key flashes red if the mic is muted
  (either an OBS input called "Mic" or the system's default source as reported by Pulseaudio).

Setup
-----

### Configuration

The OBS websocket is assumed to be on the default port.

Put the password in `$XDG_CONFIG_HOME/kaleidoscope-hostlink/obs-password`
(probably `~/.config/kaleidoscope-hostlink/obs-password`),
and change its mode to 600.
Or alternatively, set the password in the `OBS_WEBSOCKET_PASSWORD` environment variable.

### Kaleidoscope

Copy `src/Hostlink.h` to be alongside your Kaleidoscope sketch file.

In that sketch file, add FocusSerial if you don't already have it,
and also the Hostlink plugin:

```
#include "Kaleidoscope-FocusSerial.h"
#include "Hostlink.h"
```

Add `Focus` to your `KALEIDOSCOPE_INIT_PLUGINS` call if you don't already have it.
Then add `Hostlink`, `HostlinkObs`, and `HostlinkWorkscapes`,
noting that the latter two should come after all the other LED plugins you want them to override while active.

```
KALEIDOSCOPE_INIT_PLUGINS(

  // ...

  Focus,
  Hostlink,

  // ... your other LED plugins

  HostlinkObs,
  HostlinkWorkspaces,

  // ...
);
```

Build and flash the firmware to your keyboard.

### Daemon

Build via the standard Rust toolchain, or run `nix build` if you have Nix.

Start the binary manually or use the systemd unit file.

### Via Nix Home Manager

Add this package to your flake inputs, then:

```nix
{ inputs, pkgs, ... }:
{
  imports = [
    inputs.kaleidoscope-hostlink.homeManagerModules.default
  ];
  # ...
}
```
