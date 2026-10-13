//! Last-resort software dimming: a click-through translucent black
//! layer-shell surface covering the whole monitor.

use gtk::prelude::*;
use gtk::{cairo, gdk};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use super::{Backend, Kind};

pub fn is_supported() -> bool {
    gtk4_layer_shell::is_supported()
}

pub struct Overlay {
    window: gtk::Window,
}

impl Overlay {
    pub fn new(monitor: &gdk::Monitor) -> Self {
        let window = gtk::Window::new();
        window.add_css_class("dial-overlay");
        window.init_layer_shell();
        window.set_namespace(Some("dial-overlay"));
        window.set_layer(Layer::Overlay);
        window.set_monitor(Some(monitor));
        for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            window.set_anchor(edge, true);
        }
        window.set_exclusive_zone(-1);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_can_target(false);
        // An empty input region lets every click pass through to the windows below.
        window.connect_realize(|w| {
            if let Some(surface) = w.surface() {
                surface.set_input_region(Some(&cairo::Region::create()));
            }
        });
        Self { window }
    }
}

impl Backend for Overlay {
    fn kind(&self) -> Kind {
        Kind::Overlay
    }

    fn set(&mut self, pct: u8) -> Result<(), String> {
        let alpha = 1.0 - pct.min(100) as f64 / 100.0;
        self.window.set_opacity(alpha);
        self.window.set_visible(alpha > 0.0);
        Ok(())
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.window.destroy();
    }
}
