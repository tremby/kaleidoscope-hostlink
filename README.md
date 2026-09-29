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

Setup
-----

### Kaleidoscope

Copy `src/Hostlink.h` to be alongside your Kaleidoscope sketch file.

In that sketch file, add FocusSerial if you don't already have it,
and also the Hostlink plugin:

```
#include "Kaleidoscope-FocusSerial.h"
#include "Hostlink.h"
```

Add `Focus` to your `KALEIDOSCOPE_INIT_PLUGINS` call if you don't already have it.
Then add `Hostlink` and `HostlinkWorkscapes`,
noting that `HostlinkWorkpaces` should come after all the other LED plugins you want it to override while active.

```
KALEIDOSCOPE_INIT_PLUGINS(

  // ...

  Focus,
  Hostlink,

  // ... your other LED plugins

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
