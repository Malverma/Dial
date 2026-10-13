# LinuxDimmer — Specification

## 1. Overview

LinuxDimmer is a small desktop app for Arch Linux that controls screen brightness. The window shows one **vertical slider per connected monitor** and one **master slider** that moves all monitors together.

### Goals
- Change any monitor's brightness in one drag.
- Change all monitors at once with a master slider.
- Work on both laptop panels and external monitors.
- Work on Wayland (Hyprland, Sway, KDE, GNOME) and X11.
- Stay small: one window, no background daemon required, few dependencies.

### Non-Goals (v1)
- Color temperature or night-light scheduling (tools like hyprsunset and gammastep already do this).
- Automatic brightness from an ambient light sensor.
- Per-application dimming.
- Profiles or scheduled brightness.

---

## 2. User Interface

### 2.1 Layout

```
┌──────────────────────────────────────────────────────┐
│  LinuxDimmer                                    ─ ✕  │
├──────────────────────────────────────────────────────┤
│                                                      │
│    ALL      │   eDP-1      DP-1       HDMI-A-1       │
│             │                                        │
│    ┃        │    ┃          ┃          ┃             │
│    ┃        │    ┃          █          ┃             │
│    █        │    █          ┃          ┃             │
│    ┃        │    ┃          ┃          █             │
│    ┃        │    ┃          ┃          ┃             │
│             │                                        │
│    70%      │   70%        85%        40%            │
│             │  Backlight   DDC/CI     Gamma          │
│             │                                        │
└──────────────────────────────────────────────────────┘
```

- **Master slider ("ALL")** on the far left, separated from the others by a vertical divider.
- **One vertical slider per monitor**, ordered left to right by each monitor's position in the desktop layout.
- Each slider has:
  - **Label**: the connector name (e.g. `DP-1`). The tooltip shows the monitor's make and model from EDID (e.g. "Dell U2720Q").
  - **Value readout**: the current percentage, 0–100%.
  - **Backend badge**: a small caption that says how this monitor is dimmed (`Backlight`, `DDC/CI`, `Gamma`, or `Overlay`).
- Up means brighter and down means dimmer.

### 2.2 Interaction

| Input | Action |
|---|---|
| Drag a slider | Set brightness live while dragging (rate-limited, see §4.4) |
| Scroll wheel over a slider | ±5% per notch |
| Arrow Up / Down (slider focused) | ±1% |
| Page Up / Page Down | ±10% |
| Home / End | 100% / minimum |
| Double-click the value readout | Type an exact percentage |
| Tab / Shift+Tab | Move focus between sliders |

### 2.3 Master Slider Behavior

The master slider sets the **same absolute value** on every monitor.

- Moving the master to 60% sets every monitor to 60%.
- When the per-monitor sliders differ, the master shows their **average** and is drawn in an "indeterminate" style (dimmed thumb) until it is moved.
- Moving any single monitor slider afterwards updates the master's average without changing the other monitors.

> Alternative considered: relative mode, where the master scales each monitor proportionally and keeps their offsets. This is deferred to v2 as an optional setting (`master_mode = "absolute" | "relative"`).

### 2.4 Minimum Brightness Safety

- Every slider has a floor so a screen can't go fully black by accident. The default floor is **10%**, configurable per monitor.
- In gamma or overlay modes, a 0% value would make the screen unreadable. These modes enforce a hard minimum of **5%** that cannot be overridden.

### 2.5 Hotplug

When a monitor is connected or disconnected, its slider is added or removed **without restarting the app**. Detection uses udev `drm` events, plus compositor output events where available.

### 2.6 Window Behavior
- Resizable window (default 560×380) so tiling compositors such as Hyprland tile it; the monitor columns share the width equally and the sliders stretch to the window height.
- Closing the window exits the app. A tray icon is optional (§8).
- Follows the system light/dark theme (GTK/libadwaita).

---

## 3. Dimming Backends

Each monitor is assigned the **best available backend**, in this priority order:

| Priority | Backend | Applies to | Mechanism | Real hardware dimming? |
|---|---|---|---|---|
| 1 | **Backlight** | Laptop and built-in panels | `/sys/class/backlight/*/brightness` | Yes |
| 2 | **DDC/CI** | External monitors that support it | VCP code `0x10` over I²C (`/dev/i2c-*`) via libddcutil | Yes |
| 3 | **Gamma** | Any monitor | Gamma ramp scaling per output | No (software) |
| 4 | **Overlay** | Any monitor (last resort) | Click-through translucent black layer-shell surface | No (software) |

Users can override the backend per monitor in the config (§6).

### 3.1 Backlight
- Enumerate `/sys/class/backlight/` and map each device to its DRM connector through sysfs (`/sys/class/drm/card*-eDP-*/`).
- Writes go through **systemd-logind** `SetBrightness` over D-Bus (`org.freedesktop.login1.Session.SetBrightness`). This needs **no root and no udev rule**.
- Use a perceptual curve (§4.2) because raw backlight values are linear and look uneven to the eye.

### 3.2 DDC/CI
- Uses **libddcutil** (the `ddcutil` package) to read and write VCP feature `0x10` (Luminance).
- Needs the `i2c-dev` kernel module and user access to `/dev/i2c-*`. See §7.
- DDC is **slow**: about 40–100 ms per write and sometimes flaky. Writes must be:
  - Run on a **worker thread per monitor**, never on the UI thread.
  - **Coalesced**: only the most recent pending value is sent, and intermediate values are dropped.
  - Retried up to 3 times on failure. After repeated failures, the monitor falls back to Gamma and the user is notified.
- Monitors are matched to DRM connectors by EDID (manufacturer, model, serial).
- Monitor detection is done once at startup and again on hotplug. It is cached because detection is slow.

### 3.3 Gamma (Software)
Gamma scales the output's color ramp. The panel's real backlight is unchanged, so blacks stay the same and only peak brightness drops.

| Session | Implementation |
|---|---|
| wlroots compositors (Hyprland, Sway, river, etc.) | `wlr-gamma-control-unstable-v1` protocol, per output |
| KDE Plasma (Wayland) | KWin D-Bus brightness API (Plasma 6.x) where available, otherwise Overlay |
| GNOME (Wayland) | No public per-output gamma API → falls back to Overlay |
| X11 | XRandR CRTC gamma (`XRRSetCrtcGamma`) |

Notes:
- `wlr-gamma-control` allows **only one client per output**. If hyprsunset, gammastep, or wlsunset is running, LinuxDimmer detects the conflict (the protocol returns a `failed` event) and shows a warning on that monitor, offering Overlay instead.
- The gamma ramp is restored when the app exits or crashes. The protocol does this automatically when the client disconnects. On X11, an exit handler restores the saved ramp.
- Because the protocol resets the ramp on disconnect, gamma dimming **only lasts while LinuxDimmer is running**. See §5.3.

### 3.4 Overlay (Software)
- Draws a full-screen, input-transparent, translucent black surface using `wlr-layer-shell` (Wayland) or an override-redirect ARGB window (X11).
- Opacity = `1 − brightness`.
- This is the last resort because it can affect screenshots and screen recordings, and it lasts only while the app is running.

---

## 4. Brightness Model

### 4.1 Value Range
- UI values are integers from **0 to 100**.
- Each backend maps UI % to its own native range (backlight `max_brightness`, DDC max value, gamma multiplier 0.0–1.0, overlay opacity).

### 4.2 Perceptual Curve
Brightness is perceived non-linearly, so slider % is mapped to output with a gamma-like curve:

```
native = round(min_native + (max_native − min_native) × (pct/100) ^ γ)
```

- Default **γ = 2.2** for Backlight.
- Default **γ = 1.0** for DDC, because monitors usually apply their own curve.
- Default **γ = 1.0** for Gamma and Overlay.
- Configurable per monitor.

### 4.3 Reading Current Values
On startup, each slider is set from the device's **current** brightness (read back from sysfs or DDC), so the UI never jumps. Gamma and overlay monitors start at 100% unless a saved value is restored (§5.3).

### 4.4 Rate Limiting
- While a slider is dragged, values are sent at most every **16 ms** for Backlight, Gamma, and Overlay, and every **100 ms** for DDC.
- The final value on release is always sent.

---

## 5. Architecture

### 5.1 Technology Stack

| Concern | Choice | Rationale |
|---|---|---|
| Language | **Rust** | Single static binary, good threading for DDC workers, strong Wayland crates |
| UI toolkit | **GTK4 + libadwaita** (`gtk4-rs`, `libadwaita-rs`) | Native look, vertical `GtkScale`, follows theme |
| Wayland | `wayland-client`, `wayland-protocols-wlr` | Gamma control and layer-shell |
| X11 | `x11rb` (RandR) | Gamma on X11 |
| DDC/CI | `ddcutil` via FFI to `libddcutil` (or the `ddc-hi` crate as an alternative) | Most robust DDC implementation on Linux |
| D-Bus | `zbus` | logind `SetBrightness`, KWin |
| Hotplug | `udev` crate | DRM change events |
| Config | `serde` + `toml` | Human-editable config |

### 5.2 Module Layout

```
src/
├── main.rs              # Entry point, CLI arg parsing
├── app.rs               # GTK Application, window, slider widgets
├── model.rs             # Monitor struct, brightness state, master logic
├── discovery.rs         # Enumerate DRM connectors, EDID parsing, backend selection
├── hotplug.rs           # udev monitor → add/remove events
├── config.rs            # Load/save TOML config and state
├── curve.rs             # Perceptual mapping
└── backend/
    ├── mod.rs           # trait Backend
    ├── backlight.rs
    ├── ddc.rs
    ├── gamma_wlr.rs
    ├── gamma_x11.rs
    ├── kwin.rs
    └── overlay.rs
```

### 5.3 Backend Trait

```rust
pub trait Backend: Send {
    fn kind(&self) -> BackendKind;            // Backlight | Ddc | Gamma | Overlay
    fn get(&mut self) -> Result<u8>;          // current brightness 0–100
    fn set(&mut self, pct: u8) -> Result<()>; // apply brightness 0–100
    fn min_floor(&self) -> u8;                // hard minimum for this backend
    fn is_persistent(&self) -> bool;          // true if it survives app exit
}
```

**Persistence:**
- Backlight and DDC changes stay in effect after the app exits. Hardware keeps them.
- Gamma and Overlay changes stop when the app exits. For users who want software dimming to stay on, the app can be minimized to the tray (§8).

### 5.4 Threading
- **UI thread**: GTK main loop only.
- **One worker thread per DDC monitor**: receives values over a single-slot channel ("latest value wins").
- **Wayland/X11 event thread**: gamma and overlay surfaces, output events.
- **udev thread**: hotplug notifications forwarded to the UI through `glib::MainContext` channels.

---

## 6. Configuration

Location: `$XDG_CONFIG_HOME/linuxdimmer/config.toml` (default `~/.config/linuxdimmer/config.toml`).

```toml
[general]
master_mode = "absolute"     # "absolute" | "relative" (v2)
restore_on_start = true      # re-apply last brightness when the app starts
scroll_step = 5

# Per-monitor overrides, keyed by EDID identity (stable across port changes)
[[monitor]]
match = { make = "Dell Inc.", model = "DELL U2720Q", serial = "ABC123" }
name = "Left Dell"           # display label instead of connector name
backend = "ddc"              # "auto" | "backlight" | "ddc" | "gamma" | "overlay"
min = 15
curve_gamma = 1.0

[[monitor]]
match = { connector = "eDP-1" }
backend = "backlight"
min = 5
curve_gamma = 2.2
```

Last-used values are stored separately in `$XDG_STATE_HOME/linuxdimmer/state.toml`.

---

## 7. Command-Line Interface

The same binary can be used without the GUI, which is handy for keybindings (e.g. Hyprland `bind`):

```
linuxdimmer                         # open GUI
linuxdimmer list                    # list monitors, backend, current %
linuxdimmer get [MONITOR]           # print brightness
linuxdimmer set <MONITOR|all> <N>   # set absolute %, e.g. set DP-1 60
linuxdimmer set <MONITOR|all> +N    # relative, e.g. set all +10
linuxdimmer set <MONITOR|all> -N
```

Example Hyprland bindings:

```
bind = , XF86MonBrightnessUp,   exec, linuxdimmer set all +5
bind = , XF86MonBrightnessDown, exec, linuxdimmer set all -5
```

> The CLI works for the Backlight and DDC backends. Gamma and Overlay need a running process, so CLI commands for those monitors are forwarded to the running GUI instance over D-Bus (`io.github.linuxdimmer`). If no instance is running, the command errors out with a clear message.

---

## 8. Optional Features (post-v1)
- **System tray icon** (StatusNotifierItem): scroll to change the master value; click to show or hide the window. Keeps gamma and overlay dimming active while the window is hidden.
- **Relative master mode** (§2.3).
- **Linked groups**: link two monitors so they move together.
- **Presets** ("Day", "Night", "Movie").
- **Waybar module** that outputs JSON with the current master brightness.

---

## 9. Packaging (Arch Linux)

Distributed as a `PKGBUILD` (AUR: `linuxdimmer`, plus `linuxdimmer-git`).

**Dependencies**

| Type | Packages |
|---|---|
| depends | `gtk4`, `libadwaita`, `systemd-libs`, `wayland`, `libx11`, `libxrandr` |
| optdepends | `ddcutil: external monitor DDC/CI control` |
| makedepends | `rust`, `cargo` |

**Installed files**
```
/usr/bin/linuxdimmer
/usr/share/applications/io.github.linuxdimmer.desktop
/usr/share/icons/hicolor/scalable/apps/io.github.linuxdimmer.svg
/usr/lib/modules-load.d/linuxdimmer-i2c.conf     # contains: i2c-dev
/usr/lib/udev/rules.d/60-linuxdimmer-i2c.rules   # see below
```

**I²C permissions for DDC/CI.** `ddcutil` already ships `60-ddcutil-i2c.rules`, which uses `uaccess` to give the logged-in seat user access. LinuxDimmer relies on that rule when `ddcutil` is installed and only installs its own equivalent if it is missing:

```
KERNEL=="i2c-[0-9]*", SUBSYSTEM=="i2c-dev", TAG+="uaccess"
```

After installing, the user may need to reboot or run `sudo modprobe i2c-dev` once.

**No root or setuid is ever required at runtime.**

---

## 10. Error Handling & UX Messages

| Situation | Behavior |
|---|---|
| No permission for `/dev/i2c-*` | Badge shows "Gamma", with an info icon linking to "Enable DDC/CI" help text |
| Monitor doesn't support DDC | Silently use Gamma (or Overlay) |
| DDC write fails 3× | Toast: "DP-1 stopped responding to DDC; switched to software dimming" |
| Gamma control taken by another app | Warning icon on slider: "Gamma is in use by another app (e.g. hyprsunset). Use overlay instead?" |
| No monitors found | Empty-state page with a diagnostics button that runs the equivalent of `linuxdimmer list --verbose` |

---

## 11. Testing

- **Unit tests**: curve mapping, master averaging, config parsing, EDID matching.
- **Backend mocks**: a `FakeBackend` used to test the UI model without hardware.
- **Manual test matrix**:

| Session | Laptop panel | External DDC | External non-DDC |
|---|---|---|---|
| Hyprland | ☐ | ☐ | ☐ |
| Sway | ☐ | ☐ | ☐ |
| KDE Plasma 6 | ☐ | ☐ | ☐ |
| GNOME | ☐ | ☐ | ☐ |
| X11 (i3 / Xfce) | ☐ | ☐ | ☐ |

- Hotplug: connect and disconnect a monitor while dragging the master slider; the app must not crash or leave a stale slider.
- Crash safety: `kill -9` the app while gamma dimming is active and confirm the screen returns to normal brightness.

---

## 12. Milestones

1. **M1 – Core**: monitor discovery, Backlight + Gamma (wlroots) backends, GUI with per-monitor and master sliders.
2. **M2 – DDC/CI**: libddcutil integration, worker threads, coalescing, fallback.
3. **M3 – Polish**: config file, state restore, hotplug, CLI.
4. **M4 – Coverage**: X11 gamma, KWin, Overlay backend.
5. **M5 – Release**: PKGBUILD, AUR publish, README with setup instructions.

---

## 13. Implementation Notes (v0.1.0)

Where the first build differs from the plan above:

- **DDC/CI** uses the pure-Rust `ddc-i2c` crate instead of FFI to libddcutil, so ddcutil is no longer a dependency. When sysfs doesn't link a connector to an I²C bus (as with proprietary NVIDIA drivers), the bus is found by matching EDIDs. SMBus adapters are never probed.
- **Hotplug** uses GDK's monitor list (`items-changed`) instead of a separate udev thread.
- **CLI forwarding** uses GApplication's built-in single-instance D-Bus command line instead of a custom interface.
- **Gamma conflicts** switch the monitor to Overlay automatically and show a toast, instead of asking first.
- **Not yet implemented:** X11 gamma/overlay, the KWin backend, the empty-state diagnostics button, and the tray icon (§8).
- **Packaging:** the local PKGBUILD lives in `packaging/arch/`.
