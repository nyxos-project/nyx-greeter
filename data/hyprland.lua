-- The Hyprland session greetd runs Nyx Greeter in: no animations, nothing but the
-- greeter and a screen reader. Hyprland exits when the greeter does, so greetd can start
-- the user's session (or show the greeter again).

hl.monitor({ output = "", mode = "preferred", position = "auto", scale = "auto" })

hl.env("XCURSOR_SIZE", "24")

hl.config({
  general = { border_size = 0, gaps_in = 0, gaps_out = 0 },
  decoration = { shadow = { enabled = false }, blur = { enabled = false } },
  animations = { enabled = false },
  input = { kb_layout = "us", touchpad = { natural_scroll = true } },
  misc = {
    disable_hyprland_logo = true,
    disable_splash_rendering = true,
    force_default_wallpaper = 0,
    background_color = "rgb(09090a)",
  },
  ecosystem = { no_update_news = true, no_donation_nag = true },
})

-- Super+Alt+S starts and stops the Orca screen reader, as on GNOME's login screen.
hl.bind("SUPER + ALT + S", hl.dsp.exec_cmd([[sh -c 'pkill -f "bin/\.?orca" || exec orca --replace']]), { description = "Screen reader" })

hl.on("hyprland.start", function()
  hl.exec_cmd("sh -c 'nyx-greeter; hyprctl eval \"hl.dispatch(hl.dsp.exit())\"'")
end)
