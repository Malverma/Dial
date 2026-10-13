# Dimmer

Per-monitor brightness control for Arch Linux. One vertical slider per monitor, plus an **ALL** slider that sets every monitor at once. See [spec.md](spec.md) for the full design.

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

Dependencies: `gtk4`, `libadwaita`, `gtk4-layer-shell`, `rust`.

## DDC/CI setup (external monitors)

Without this, external monitors fall back to software (gamma) dimming. That works, but it only lowers the image's brightness, not the panel's backlight.

```sh
sudo modprobe i2c-dev   # the package also loads it at every boot
```

The package installs a udev rule (`TAG+="uaccess"`) that lets the logged-in user open `/dev/i2c-*`. Log out and back in, or reboot, after installing. Hover over a slider's badge to see which backend it uses and why. `dimmer list --verbose` shows the same information.

On proprietary NVIDIA drivers, Dimmer finds each monitor's I²C bus by matching EDIDs, the same way ddcutil does. DDC/CI also has to be turned on in the monitor's OSD menu.

## Command line

```
dimmer                         Open the window
dimmer list [--verbose]        List monitors, backend and brightness
dimmer get [MONITOR]           Print brightness
dimmer set <MONITOR|all> <N>   Set brightness: 60, +10 or -10
```

When the window is open, commands are forwarded to it, so they also work for gamma and overlay monitors. When it isn't open, only backlight and DDC/CI monitors can be changed.

Hyprland keybindings:

```
bind = , XF86MonBrightnessUp,   exec, dimmer set all +5
bind = , XF86MonBrightnessDown, exec, dimmer set all -5
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

When monitors are set to different levels, the ALL slider shows their average with a faded thumb. Moving it sets every monitor to the same value.

## Configuration

`~/.config/dimmer/config.toml` (all optional):

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

Last-used values are saved in `~/.local/state/dimmer/state.toml`.

## Notes

- Gamma and overlay dimming last only while Dimmer is running. The compositor restores normal brightness when it exits, even after a crash.
- Hyprland's night light (hyprsunset) uses a different protocol, so it works alongside Dimmer. gammastep and wlsunset take over the gamma control, so Dimmer switches those monitors to the overlay.
- On GNOME there is no gamma or layer-shell protocol, so only backlight and DDC/CI work. X11 software dimming isn't implemented yet.

## Development

```sh
cargo test
cargo clippy
```
