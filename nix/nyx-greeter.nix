# Nyx Greeter for nyx-packages (copy to pkgs/nyx-greeter.nix). Inputs: this repo as
# `src`, nyx-design as `design` (for the mark). The Hyprland session config goes to
# share/nyx-greeter/hyprland.lua, with nyx-greeter on its PATH.
{
  nyx,
  src,
  design,
  gtk4-layer-shell,
  librsvg,
  hyprland,
}:
nyx.buildApp {
  pname = "nyx-greeter";
  inherit src;
  cargoBuildFlags = [ "-p" "nyx-greeter" ];
  buildInputs = [ gtk4-layer-shell ];
  nativeBuildInputs = [ librsvg ];
  runtimeInputs = [ hyprland ];
  postInstall = ''
    install -Dm644 data/hyprland.lua $out/share/nyx-greeter/hyprland.lua
    substituteInPlace $out/share/nyx-greeter/hyprland.lua \
      --replace-fail "sh -c 'nyx-greeter;" "sh -c '$out/bin/nyx-greeter;"
    rsvg-convert -w 192 -h 192 ${design}/brand/nyx-mark-silver.svg \
      -o $out/share/nyx-greeter/mark.png
  '';
  meta = {
    description = "Nyx Greeter: the NyxOS login screen, for greetd";
    homepage = "https://github.com/nyxos-project/nyx-greeter";
    mainProgram = "nyx-greeter";
  };
}
