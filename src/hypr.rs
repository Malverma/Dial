//! Hyprland integration.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

/// Hyprland's event socket, when running under Hyprland.
pub fn event_socket() -> Option<UnixStream> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let sig = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    UnixStream::connect(PathBuf::from(runtime).join("hypr").join(sig).join(".socket2.sock")).ok()
}

/// Whenever our window is toggled to floating, resize it to a `size`-pixel
/// square and center it. Hyprland otherwise keeps the tiled size, and window
/// rules for size only apply when a window opens.
pub fn float_as_square(size: u32) {
    let Some(socket) = event_socket() else { return };
    std::thread::spawn(move || {
        for line in BufReader::new(socket).lines().map_while(Result::ok) {
            // "changefloatingmode>>ADDRESS,1"; the address has no 0x prefix.
            let Some(rest) = line.strip_prefix("changefloatingmode>>") else { continue };
            let Some((addr, "1")) = rest.split_once(',') else { continue };
            let addr = format!("0x{addr}");
            if own_windows().contains(&addr) {
                let batch = format!("dispatch resizewindowpixel exact {size} {size},address:{addr}; dispatch centerwindow");
                let _ = Command::new("hyprctl").args(["--batch", &batch]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
        }
    });
}

/// Addresses of this process's windows.
fn own_windows() -> Vec<String> {
    #[derive(Deserialize)]
    struct Client {
        address: String,
        pid: i64,
    }
    let Ok(out) = Command::new("hyprctl").args(["-j", "clients"]).stderr(Stdio::null()).output() else { return Vec::new() };
    let own = std::process::id() as i64;
    serde_json::from_slice::<Vec<Client>>(&out.stdout)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.pid == own)
        .map(|c| c.address)
        .collect()
}
