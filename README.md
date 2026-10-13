# Dial

Brightness and audio control for Arch Linux and Hyprland, in one transparent window:

- **DIM** sets every monitor's brightness at once.
- **One volume slider per app**, next to DIM. Every instance of an app shares one slider (two Firefox windows → one slider). Open apps get a slider even before they play audio, so their audio starts at that level.
- **MIC** sets the default microphone's gain from 0 to 200%, next to one slider per audio output.
- **Spectrum meter** along the bottom shows whatever output is playing, preferring the one Firefox plays through. It grows with the window but stays half a slider row tall.

See [spec.md](spec.md) for the brightness design.

Each monitor is dimmed with the best method available:

| Backend | Used for | Real hardware dimming | Survives app exit |
|---|---|---|---|
| Backlight | Laptop panels (through systemd-logind, no root needed) | yes | yes |
| DDC/CI | External monitors that support it (`/dev/i2c-*`) | yes | yes |
| Gamma | Any monitor on wlroots compositors (Hyprland, Sway, …) | no | no |
| Overlay | Fallback: a click-through dark layer-shell surface | no | no |

## Build & install

```sh
cd packaging/arch
makepkg -si
```

Or run it straight from the checkout:

```sh
cargo run --release
```

Dependencies: `gtk4`, `libadwaita`, `gtk4-layer-shell`, `libpulse` (for `pactl`/`parec`, works with PipeWire), `rust`.

## DDC/CI setup (external monitors)

Without this, external monitors fall back to software (gamma) dimming. That works, but it only lowers the image's brightness, not the panel's backlight.

```sh
sudo modprobe i2c-dev   # the package also loads it at every boot
```

The package installs a udev rule (`TAG+="uaccess"`) that lets the logged-in user open `/dev/i2c-*`. Log out and back in, or reboot, after installing. `dial list --verbose` shows which backend each monitor uses and why.

On proprietary NVIDIA drivers, Dial finds each monitor's I²C bus by matching EDIDs, the same way ddcutil does. DDC/CI also has to be turned on in the monitor's OSD menu.

## Command line

```
dial                         Open the window
dial list [--verbose]        List monitors, backend and brightness
dial get [MONITOR]           Print brightness
dial set <MONITOR|all> <N>   Set brightness: 60, +10 or -10
```

When the window is open, commands are forwarded to it, so they also work for gamma and overlay monitors. When it isn't open, only backlight and DDC/CI monitors can be changed.

Hyprland keybindings:

```
bind = , XF86MonBrightnessUp,   exec, dial set all +5
bind = , XF86MonBrightnessDown, exec, dial set all -5
```

## Controls

| Input | Action |
|---|---|
| Drag | Set brightness live |
| Scroll wheel | ±5% (`scroll_step`) |
| ↑ / ↓ | ±1% |
| Page Up / Page Down | ±10% |
| Home / End | 100% / minimum |
| Double-click the percentage | Type an exact value |

When monitors are set to different levels, the DIM slider shows their average with a faded thumb. Moving it sets every monitor to the same value.

## Audio

Dial holds volumes where you set them. If something else changes an output's volume, the mic gain, or a locked app's volume, Dial puts it back. Players that reset the volume on every new track and Bluetooth headsets that reset theirs are both handled. Volume keys and other mixers are reverted too.

The settings button (top right) has:

- **Default Output**: make an output the default whenever it connects (e.g. Bluetooth headphones). You can still switch by hand while it's connected.
- **Apps Without Audio**: show sliders only for apps that are playing audio.
- **Hold App Volumes**: turn the lock off for apps that manage their own volume. Vesktop is off by default. While an unlocked app is recording, Dial lets it change the mic gain (e.g. Discord's automatic gain). Every app Dial has seen stays in this list.
- **Close Dial**: the window has no close button of its own.

Under Hyprland, toggling Dial to floating turns it into a 480×480 square.

## Configuration

`~/.config/dial/config.toml` (all optional):

```toml
[general]
restore_on_start = true   # re-apply last software (gamma/overlay) brightness at startup
scroll_step = 5

[[monitor]]
match = { connector = "DP-1" }          # or make / model / serial from the EDID
name = "Left BenQ"
backend = "auto"                        # auto | backlight | ddc | gamma | overlay
min = 15                                # floor in %, default 10 (software backends: at least 5)
curve_gamma = 1.0                       # perceptual curve; default 2.2 for backlight, 1.0 otherwise
```

Last-used brightness, held volumes, seen apps and settings are saved in `~/.local/state/dial/state.toml`.

## Notes

- Gamma and overlay dimming last only while Dial is running. The compositor restores normal brightness when it exits, even after a crash.
- Hyprland's night light (hyprsunset) uses a different protocol, so it works alongside Dial. gammastep and wlsunset take over the gamma control, so Dial switches those monitors to the overlay.
- On GNOME there is no gamma or layer-shell protocol, so only backlight and DDC/CI work. X11 software dimming isn't implemented yet.

## Development

```sh
cargo test
cargo clippy
```
