# Packaging Nyx Greeter for NyxOS

For the session working on `nyx-packages` and `nyxos`. Nyx Greeter is pushed at
`nyxos-project/nyx-greeter`; its build was tested on Arch, not yet under Nix, and it
has only run against the fake greetd (`--demo`), never as the real login screen.

## nyx-packages

1. `flake.nix` inputs: `nyx-greeter = { url = "github:nyxos-project/nyx-greeter"; flake = false; };`
   (`nyx-design` is already an input).
2. Copy [nyx-greeter.nix](nyx-greeter.nix) to `pkgs/nyx-greeter.nix`, add
   `"nyx-greeter"` to the `fromSource` list in `pkgs/default.nix`, and pass
   `design = inputs.nyx-design;`.
3. A check: `nyx.testWorkspace { pname = "nyx-greeter-tests"; src = inputs.nyx-greeter;
   buildInputs = [ gtk4-layer-shell ]; }`. The tests include a whole login against a
   fake greetd socket, so they need nothing from the host.

## nyxos

`modules/nixos/desktop.nix` replaces tuigreet:

```nix
services.greetd = {
  enable = true;
  settings.default_session.command =
    "${lib.getExe pkgs.hyprland} --config ${pkgs.nyx-greeter}/share/nyx-greeter/hyprland.lua";
  # initial_session (autoLogin) stays as it is.
};

environment.etc."nyx-greeter/config.toml".text = ''
  theme = "${theme.id}"
  wallpaper = "${pkgs.nyx-wallpapers}/share/backgrounds/nyx/nyx-dark/monochrome/nyx_monochrome_02_4K.jpg"
  mark = "${pkgs.nyx-greeter}/share/nyx-greeter/mark.png"
  default_session = "hyprland-uwsm"
'';

# Who logged in last, and with which session.
systemd.tmpfiles.rules = [ "d /var/cache/nyx-greeter 0755 greeter greeter -" ];
```

- The wallpaper path depends on how `nyx-wallpapers` gets packaged; any readable JPG
  or PNG works, and none gives the theme's plain background.
- The greeter user needs the fonts (Inter) and icons (Adwaita) the desktop already
  installs system-wide.
- Avatars come from `/var/lib/AccountsService/icons/<user>` or `~/.face` (if the
  greeter user can read it).
- Fingerprint login works when the greetd PAM stack has `pam_fprintd`; the greeter
  shows PAM's prompts as they come.

## On the VM, check

- The greeter shows on every display: clock, wallpaper, and the card on the largest.
- A wrong password shakes the card and says so; the right one starts Hyprland.
- After logging out, the last user and session are preselected.
- Suspend, restart and shut down work from the greeter (logind allows the active
  local session by default).
