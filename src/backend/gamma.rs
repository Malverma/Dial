//! Software dimming by scaling each output's gamma ramp through
//! `wlr-gamma-control-unstable-v1` (Hyprland, Sway, river, …).
//!
//! The compositor restores the original ramp when a control object is
//! destroyed or this client disconnects, so a crash never leaves screens dim.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::rc::{Rc, Weak};

use gtk::glib;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

use super::{Backend, Kind};

struct Output {
    global: u32,
    _wl: wl_output::WlOutput,
    name: Option<String>,
    control: Option<ZwlrGammaControlV1>,
    size: Option<u32>,
    applied: Option<f64>,
    failed: bool,
}

struct State {
    manager: ZwlrGammaControlManagerV1,
    outputs: Vec<Output>,
    /// Brightness multiplier wanted per connector; absent means 1.0.
    desired: HashMap<String, f64>,
    newly_failed: Vec<String>,
}

struct Inner {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    on_failed: Option<Rc<dyn Fn(String)>>,
}

pub struct GammaManager {
    inner: Rc<RefCell<Inner>>,
}

impl GammaManager {
    /// Connects to the compositor. Returns `None` when it lacks gamma control.
    pub fn connect() -> Option<Rc<Self>> {
        let conn = Connection::connect_to_env().ok()?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).ok()?;
        let qh = queue.handle();
        let manager: ZwlrGammaControlManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;
        let mut state = State { manager, outputs: Vec::new(), desired: HashMap::new(), newly_failed: Vec::new() };
        globals.contents().with_list(|list| {
            for g in list.iter().filter(|g| g.interface == wl_output::WlOutput::interface().name) {
                state.add_output(globals.registry(), &qh, g.name, g.version);
            }
        });
        queue.roundtrip(&mut state).ok()?;

        let inner = Rc::new(RefCell::new(Inner { conn, queue, state, on_failed: None }));
        let fd = inner.borrow().conn.backend().poll_fd().as_raw_fd();
        let weak: Weak<RefCell<Inner>> = Rc::downgrade(&inner);
        glib_unix::unix_fd_add_local(fd, glib::IOCondition::IN, move |_, _| {
            let Some(inner) = weak.upgrade() else { return glib::ControlFlow::Break };
            match Inner::dispatch(&inner) {
                Ok(()) => glib::ControlFlow::Continue,
                Err(e) => {
                    eprintln!("dimmer: lost gamma-control connection: {e}");
                    glib::ControlFlow::Break
                }
            }
        });
        Some(Rc::new(Self { inner }))
    }

    /// Called (from the main loop) when the compositor refuses control of an output,
    /// usually because another app such as gammastep or wlsunset holds it.
    pub fn set_on_failed(&self, f: impl Fn(String) + 'static) {
        self.inner.borrow_mut().on_failed = Some(Rc::new(f));
    }

    pub fn is_failed(&self, connector: &str) -> bool {
        self.inner.borrow().state.outputs.iter().any(|o| o.failed && o.name.as_deref() == Some(connector))
    }

    pub fn set(&self, connector: &str, multiplier: f64) {
        let mut inner = self.inner.borrow_mut();
        let Inner { conn, queue, state, .. } = &mut *inner;
        state.desired.insert(connector.to_string(), multiplier.clamp(0.0, 1.0));
        state.reconcile(&queue.handle());
        let _ = conn.flush();
    }

    pub fn reset(&self, connector: &str) {
        let mut inner = self.inner.borrow_mut();
        let Inner { conn, queue, state, .. } = &mut *inner;
        state.desired.remove(connector);
        state.reconcile(&queue.handle());
        let _ = conn.flush();
    }

    /// A backend for one connector that shares this connection.
    pub fn backend(self: &Rc<Self>, connector: &str) -> GammaBackend {
        GammaBackend { manager: self.clone(), connector: connector.to_string() }
    }
}

impl Inner {
    fn dispatch(this: &Rc<RefCell<Inner>>) -> Result<(), String> {
        let (failed, callback) = {
            let mut inner = this.borrow_mut();
            let Inner { conn, queue, state, on_failed } = &mut *inner;
            if let Some(guard) = queue.prepare_read() {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.to_string()),
                }
            }
            queue.dispatch_pending(state).map_err(|e| e.to_string())?;
            state.reconcile(&queue.handle());
            conn.flush().map_err(|e| e.to_string())?;
            (std::mem::take(&mut state.newly_failed), on_failed.clone())
        };
        if let Some(cb) = callback {
            for name in failed {
                let cb = cb.clone();
                glib::idle_add_local_once(move || cb(name));
            }
        }
        Ok(())
    }
}

impl State {
    fn add_output(&mut self, registry: &wl_registry::WlRegistry, qh: &QueueHandle<State>, global: u32, version: u32) {
        // Version 4 is needed for the `name` event that gives the connector.
        let wl = registry.bind::<wl_output::WlOutput, _, _>(global, version.min(4), qh, global);
        self.outputs.push(Output { global, _wl: wl, name: None, control: None, size: None, applied: None, failed: false });
    }

    fn output_mut(&mut self, global: u32) -> Option<&mut Output> {
        self.outputs.iter_mut().find(|o| o.global == global)
    }

    /// Bring every output's gamma control in line with `desired`.
    fn reconcile(&mut self, qh: &QueueHandle<State>) {
        for o in &mut self.outputs {
            let Some(name) = &o.name else { continue };
            if o.failed {
                continue;
            }
            let want = self.desired.get(name).copied().unwrap_or(1.0);
            if want >= 0.999 {
                // Destroying the control restores the original ramp and frees
                // the output for other gamma tools.
                if let Some(c) = o.control.take() {
                    c.destroy();
                }
                o.size = None;
                o.applied = None;
                continue;
            }
            let Some(control) = &o.control else {
                o.control = Some(self.manager.get_gamma_control(&o._wl, qh, o.global));
                continue;
            };
            if let Some(size) = o.size
                && o.applied != Some(want) {
                    match write_ramp(control, size, want) {
                        Ok(()) => o.applied = Some(want),
                        Err(e) => eprintln!("dimmer: gamma for {name}: {e}"),
                    }
                }
        }
    }
}

/// Send a linear ramp scaled by `multiplier`: three u16 tables (R, G, B).
fn write_ramp(control: &ZwlrGammaControlV1, size: u32, multiplier: f64) -> std::io::Result<()> {
    let size = size as usize;
    let mut table = Vec::with_capacity(size * 3 * 2);
    for _ in 0..3 {
        for i in 0..size {
            let t = if size > 1 { i as f64 / (size - 1) as f64 } else { 1.0 };
            let v = (t * multiplier * 65535.0).round() as u16;
            table.extend_from_slice(&v.to_ne_bytes());
        }
    }
    let fd = rustix::fs::memfd_create("dimmer-gamma", rustix::fs::MemfdFlags::CLOEXEC)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(&table)?;
    file.seek(SeekFrom::Start(0))?;
    control.set_gamma(file.as_fd());
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } if interface == wl_output::WlOutput::interface().name => {
                state.add_output(registry, qh, name, version);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(pos) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(pos);
                    if let Some(c) = o.control {
                        c.destroy();
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Name { name } = event
            && let Some(o) = state.output_mut(*global) {
                o.name = Some(name);
            }
    }
}

impl Dispatch<ZwlrGammaControlManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrGammaControlManagerV1, _: <ZwlrGammaControlManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrGammaControlV1, u32> for State {
    fn event(state: &mut Self, _: &ZwlrGammaControlV1, event: zwlr_gamma_control_v1::Event, global: &u32, _: &Connection, _: &QueueHandle<Self>) {
        let Some(o) = state.output_mut(*global) else { return };
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => o.size = Some(size),
            zwlr_gamma_control_v1::Event::Failed => {
                if let Some(c) = o.control.take() {
                    c.destroy();
                }
                o.failed = true;
                o.size = None;
                if let Some(name) = o.name.clone() {
                    state.newly_failed.push(name);
                }
            }
            _ => {}
        }
    }
}

pub struct GammaBackend {
    manager: Rc<GammaManager>,
    connector: String,
}

impl Backend for GammaBackend {
    fn kind(&self) -> Kind {
        Kind::Gamma
    }

    fn set(&mut self, pct: u8) -> Result<(), String> {
        self.manager.set(&self.connector, pct as f64 / 100.0);
        Ok(())
    }
}

impl Drop for GammaBackend {
    fn drop(&mut self) {
        self.manager.reset(&self.connector);
    }
}
