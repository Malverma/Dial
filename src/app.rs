//! The GTK window: one vertical slider per monitor plus a master slider.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::backend::ddc::{self, DdcEvent, DdcHandle};
use crate::backend::gamma::GammaManager;
use crate::backend::{backlight::Backlight, overlay, overlay::Overlay, Backend, Kind};
use crate::cli::{Controller, Row};
use crate::config::{BackendPref, Config, MonitorSettings, State, DEFAULT_MIN};
use crate::discovery::{self, MonitorInfo};
use crate::model::{self, Target};

const THROTTLE: Duration = Duration::from_millis(16);
const SAVE_DELAY: Duration = Duration::from_millis(500);
const HOTPLUG_DELAY: Duration = Duration::from_millis(300);

const CSS: &str = "
.linuxdimmer-overlay { background: black; }
scale.indeterminate slider { opacity: 0.45; }
.column-name { font-weight: bold; }
";

mod libc_signal {
    pub const SIGINT: i32 = 2;
    pub const SIGTERM: i32 = 15;
}

pub type AppRef = Rc<RefCell<App>>;

enum Status {
    Probing(DdcHandle),
    Active(Box<dyn Backend>),
    Unsupported,
}

struct Monitor {
    id: u64,
    info: MonitorInfo,
    gdk: gdk::Monitor,
    settings: MonitorSettings,
    status: Status,
    /// Why this backend was chosen, or what went wrong; shown as a tooltip.
    note: Option<String>,
    /// Device in use, e.g. `/dev/i2c-5` or `intel_backlight`.
    detail: Option<String>,
    value: u8,
    floor: u8,
    column: Column,
    last_apply: Option<Instant>,
    apply_scheduled: bool,
}

impl Monitor {
    fn kind(&self) -> Option<Kind> {
        match &self.status {
            Status::Active(b) => Some(b.kind()),
            _ => None,
        }
    }

    fn label(&self) -> String {
        self.settings.name.clone().unwrap_or_else(|| self.info.connector.clone())
    }

    fn curve(&self, kind: Kind) -> f64 {
        self.settings.curve_gamma.unwrap_or(kind.default_curve())
    }

    fn badge_text(&self) -> &'static str {
        match &self.status {
            Status::Probing(_) => "Probing…",
            Status::Active(b) => b.kind().label(),
            Status::Unsupported => "Unsupported",
        }
    }

    fn refresh_column(&self) {
        let c = &self.column;
        c.badge.set_label(self.badge_text());
        let mut tip = self.info.pretty_name();
        if let Some(d) = &self.detail {
            tip.push_str(&format!("\n{d}"));
        }
        if let Some(n) = &self.note {
            tip.push_str(&format!("\n{n}"));
        }
        c.root.set_tooltip_text(Some(&tip));
        let active = matches!(self.status, Status::Active(_));
        c.scale.set_sensitive(active);
        c.value_stack.set_sensitive(active);
        if active {
            c.set_floor(self.floor);
            c.set_value(self.value);
        } else {
            c.value.set_label("–");
        }
    }
}

/// One labelled vertical slider.
struct Column {
    root: gtk::Box,
    scale: gtk::Scale,
    value_stack: gtk::Stack,
    value: gtk::Label,
    badge: gtk::Label,
}

impl Column {
    fn new(title: &str, step: u8) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_width_request(84);

        let name = gtk::Label::new(Some(title));
        name.add_css_class("column-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(12);

        let scale = gtk::Scale::with_range(gtk::Orientation::Vertical, 0.0, 100.0, 1.0);
        scale.set_inverted(true);
        scale.set_draw_value(false);
        scale.set_round_digits(0);
        scale.set_vexpand(true);
        scale.set_halign(gtk::Align::Center);
        scale.set_size_request(-1, 200);
        scale.adjustment().set_page_increment(10.0);

        let value = gtk::Label::new(Some("–"));
        value.add_css_class("numeric");
        value.set_tooltip_text(Some("Double-click to type a value"));
        let entry = gtk::Entry::new();
        entry.set_width_chars(4);
        entry.set_max_width_chars(4);
        entry.set_input_purpose(gtk::InputPurpose::Digits);
        EditableExt::set_alignment(&entry, 0.5);
        let value_stack = gtk::Stack::new();
        value_stack.add_named(&value, Some("label"));
        value_stack.add_named(&entry, Some("entry"));
        value_stack.set_hhomogeneous(false);

        let badge = gtk::Label::new(None);
        badge.add_css_class("caption");
        badge.add_css_class("dim-label");

        root.append(&name);
        root.append(&scale);
        root.append(&value_stack);
        root.append(&badge);

        let col = Self { root, scale, value_stack, value, badge };
        col.connect_input(&entry, step);
        col
    }

    /// Scroll, keyboard and type-a-value handling. These only move the slider;
    /// the App reacts through `value-changed`.
    fn connect_input(&self, entry: &gtk::Entry, step: u8) {
        let scale = self.scale.clone();
        let nudge = move |s: &gtk::Scale, delta: f64| s.set_value((s.value() + delta).round());

        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let s = scale.clone();
        scroll.connect_scroll(move |_, _, dy| {
            nudge(&s, -dy * step as f64);
            glib::Propagation::Stop
        });
        scale.add_controller(scroll);

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let s = scale.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            let lower = s.adjustment().lower();
            match key {
                gdk::Key::Up | gdk::Key::KP_Up => nudge(&s, 1.0),
                gdk::Key::Down | gdk::Key::KP_Down => nudge(&s, -1.0),
                gdk::Key::Page_Up | gdk::Key::KP_Page_Up => nudge(&s, 10.0),
                gdk::Key::Page_Down | gdk::Key::KP_Page_Down => nudge(&s, -10.0),
                gdk::Key::Home | gdk::Key::KP_Home => s.set_value(100.0),
                gdk::Key::End | gdk::Key::KP_End => s.set_value(lower),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        scale.add_controller(keys);

        let stack = self.value_stack.clone();
        let click = gtk::GestureClick::new();
        let (st, en, s) = (stack.clone(), entry.clone(), scale.clone());
        click.connect_pressed(move |_, n, _, _| {
            if n == 2 && s.is_sensitive() {
                en.set_text(&format!("{}", s.value() as u8));
                st.set_visible_child_name("entry");
                en.grab_focus();
                en.select_region(0, -1);
            }
        });
        self.value.add_controller(click);

        let (st, s) = (stack.clone(), scale.clone());
        entry.connect_activate(move |e| {
            if let Ok(v) = e.text().trim().trim_end_matches('%').parse::<f64>() {
                s.set_value(v.clamp(0.0, 100.0));
            }
            st.set_visible_child_name("label");
            s.grab_focus();
        });

        let cancel = gtk::EventControllerKey::new();
        let st = stack.clone();
        cancel.connect_key_pressed(move |_, key, _, _| {
            if key == gdk::Key::Escape {
                st.set_visible_child_name("label");
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(cancel);

        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| stack.set_visible_child_name("label"));
        entry.add_controller(focus);
    }

    /// Set the slider without the change being treated as user input
    /// (callers hold the App borrow, so the value-changed handler skips it).
    fn set_value(&self, v: u8) {
        self.scale.set_value(v as f64);
        self.value.set_label(&format!("{v}%"));
    }

    fn set_floor(&self, floor: u8) {
        self.scale.adjustment().set_lower(floor as f64);
    }
}

pub struct App {
    weak: Weak<RefCell<App>>,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    monitors_box: gtk::Box,
    master: Column,
    monitors: Vec<Monitor>,
    gamma: Option<Rc<GammaManager>>,
    config: Config,
    state: State,
    ddc_tx: async_channel::Sender<DdcEvent>,
    next_id: u64,
    save_scheduled: bool,
}

impl App {
    pub fn new(application: &adw::Application) -> AppRef {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }

        let config = Config::load();
        let state = State::load();
        let gamma = GammaManager::connect();

        let master = Column::new("ALL", config.general.scroll_step);
        master.badge.set_label("All monitors");
        master.root.set_tooltip_text(Some("Set every monitor to the same brightness"));

        // Monitor columns share the leftover width equally so the sliders fill a tiled window.
        let monitors_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        monitors_box.set_homogeneous(true);
        monitors_box.set_hexpand(true);
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        content.append(&master.root);
        content.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        content.append(&monitors_box);

        let empty = adw::StatusPage::builder()
            .icon_name("video-display-symbolic")
            .title("No Monitors Found")
            .description("Run <tt>linuxdimmer list --verbose</tt> in a terminal for diagnostics.")
            .build();

        let stack = gtk::Stack::new();
        stack.add_named(&content, Some("content"));
        stack.add_named(&empty, Some("empty"));

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&stack));

        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&toasts));

        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("LinuxDimmer")
            // Resizable so Hyprland tiles it; it floats windows whose min and max size match.
            .default_width(560)
            .default_height(380)
            .content(&view)
            .build();

        let (ddc_tx, ddc_rx) = async_channel::unbounded();

        let app = Rc::new_cyclic(|weak| {
            RefCell::new(App {
                weak: weak.clone(),
                window: window.clone(),
                toasts,
                stack,
                monitors_box,
                master,
                monitors: Vec::new(),
                gamma,
                config,
                state,
                ddc_tx,
                next_id: 0,
                save_scheduled: false,
            })
        });

        {
            let a = app.borrow();
            let weak = Rc::downgrade(&app);
            a.master.scale.connect_value_changed(move |s| {
                with_app(&weak, |app| app.set_all(s.value() as u8));
            });
            if let Some(gamma) = &a.gamma {
                let weak = Rc::downgrade(&app);
                gamma.set_on_failed(move |name| with_app(&weak, |app| app.on_gamma_failed(&name)));
            }
        }

        let weak = Rc::downgrade(&app);
        glib::spawn_future_local(async move {
            while let Ok(ev) = ddc_rx.recv().await {
                let Some(app) = weak.upgrade() else { break };
                app.borrow_mut().on_ddc_event(ev);
            }
        });

        if let Some(display) = gdk::Display::default() {
            let weak = Rc::downgrade(&app);
            let scheduled = Rc::new(Cell::new(false));
            display.monitors().connect_items_changed(move |_, _, _, _| {
                if scheduled.replace(true) {
                    return;
                }
                let (weak, scheduled) = (weak.clone(), scheduled.clone());
                glib::timeout_add_local_once(HOTPLUG_DELAY, move || {
                    scheduled.set(false);
                    with_app(&weak, |app| app.refresh_monitors());
                });
            });
        }

        let weak = Rc::downgrade(&app);
        window.connect_close_request(move |_| {
            with_app(&weak, |app| app.save_now());
            glib::Propagation::Proceed
        });

        // Save on SIGTERM/SIGINT too; the compositor restores gamma when we exit.
        for signal in [libc_signal::SIGTERM, libc_signal::SIGINT] {
            let (weak, application) = (Rc::downgrade(&app), application.clone());
            glib_unix::unix_signal_add_local(signal, move || {
                with_app(&weak, |app| app.save_now());
                application.quit();
                glib::ControlFlow::Break
            });
        }

        app.borrow_mut().refresh_monitors();
        window.present();
        app
    }

    pub fn present(&self) {
        self.window.present();
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.monitors.iter().position(|m| m.id == id)
    }

    /// Sync the monitor list with GDK: add new monitors, drop removed ones, reorder.
    fn refresh_monitors(&mut self) {
        let current = discovery::gdk_monitors();

        let monitors_box = &self.monitors_box;
        self.monitors.retain(|m| {
            let keep = current.iter().any(|(i, g)| i.connector == m.info.connector && *g == m.gdk);
            if !keep {
                monitors_box.remove(&m.column.root);
            }
            keep
        });

        let mut added = false;
        for (info, gdk_monitor) in current {
            if let Some(m) = self.monitors.iter_mut().find(|m| m.info.connector == info.connector) {
                m.info = info;
                continue;
            }
            if !added {
                ddc::invalidate_bus_cache();
                added = true;
            }
            self.add_monitor(info, gdk_monitor);
        }

        self.monitors.sort_by_key(|m| (m.info.x, m.info.y));
        let mut prev: Option<gtk::Widget> = None;
        for m in &self.monitors {
            self.monitors_box.reorder_child_after(&m.column.root, prev.as_ref());
            prev = Some(m.column.root.clone().upcast());
        }

        self.stack.set_visible_child_name(if self.monitors.is_empty() { "empty" } else { "content" });
        self.update_master();
    }

    fn add_monitor(&mut self, info: MonitorInfo, gdk_monitor: gdk::Monitor) {
        let id = self.next_id;
        self.next_id += 1;
        let settings = self.config.settings_for(&info);
        let column = Column::new(settings.name.as_deref().unwrap_or(&info.connector), self.config.general.scroll_step);
        let weak = self.weak.clone();
        column.scale.connect_value_changed(move |s| {
            with_app(&weak, |app| {
                if let Some(idx) = app.index_of(id) {
                    app.set_monitor_value(idx, s.value() as u8, false);
                }
            });
        });
        self.monitors_box.append(&column.root);
        self.monitors.push(Monitor {
            id,
            info,
            gdk: gdk_monitor,
            settings,
            status: Status::Unsupported,
            note: None,
            detail: None,
            value: 100,
            floor: DEFAULT_MIN,
            column,
            last_apply: None,
            apply_scheduled: false,
        });
        self.choose_backend(self.monitors.len() - 1);
    }

    /// Pick the best backend for a monitor (spec §3 priority order).
    fn choose_backend(&mut self, idx: usize) {
        let connector = self.monitors[idx].info.connector.clone();
        let pref = self.monitors[idx].settings.backend;

        if matches!(pref, BackendPref::Auto | BackendPref::Backlight) {
            match discovery::backlight_for(&connector) {
                Some(dev) => {
                    let gamma = self.monitors[idx].curve(Kind::Backlight);
                    match Backlight::open(&dev, gamma) {
                        Ok((b, current)) => {
                            self.monitors[idx].detail = Some(format!("/sys/class/backlight/{}", b.device()));
                            self.activate(idx, Box::new(b), current, false);
                            return;
                        }
                        Err(e) => self.monitors[idx].note = Some(format!("Backlight unavailable: {e}")),
                    }
                }
                None if pref == BackendPref::Backlight => {
                    self.monitors[idx].note = Some("No backlight device for this monitor".into());
                }
                None => {}
            }
        }

        if matches!(pref, BackendPref::Auto | BackendPref::Ddc) {
            if discovery::i2c_dev_loaded() {
                self.start_ddc(idx);
                return;
            }
            self.monitors[idx].note = Some("DDC/CI unavailable: i2c-dev kernel module not loaded".into());
        }

        self.use_software(idx, None);
    }

    fn start_ddc(&mut self, idx: usize) {
        let m = &mut self.monitors[idx];
        let handle = DdcHandle::spawn(m.id, m.info.connector.clone(), m.curve(Kind::Ddc), self.ddc_tx.clone());
        m.status = Status::Probing(handle);
        m.refresh_column();
    }

    /// Fall back to Gamma, then Overlay. `value` keeps the current brightness;
    /// `None` restores the saved one.
    fn use_software(&mut self, idx: usize, value: Option<u8>) {
        let connector = self.monitors[idx].info.connector.clone();
        let pref = self.monitors[idx].settings.backend;
        let gamma = self.gamma.as_ref().filter(|g| pref != BackendPref::Overlay && !g.is_failed(&connector));

        let backend: Box<dyn Backend> = if let Some(g) = gamma {
            Box::new(g.backend(&connector))
        } else if overlay::is_supported() {
            Box::new(Overlay::new(&self.monitors[idx].gdk))
        } else {
            let m = &mut self.monitors[idx];
            m.status = Status::Unsupported;
            m.note = Some("No dimming method available (no DDC/CI, gamma control or layer-shell)".into());
            m.refresh_column();
            return;
        };

        let restored = self.config.general.restore_on_start.then(|| self.state.brightness.get(&connector).copied()).flatten();
        let value = value.or(restored).unwrap_or(100);
        self.monitors[idx].detail = None;
        self.activate(idx, backend, value, true);
    }

    fn activate(&mut self, idx: usize, backend: Box<dyn Backend>, value: u8, apply: bool) {
        let m = &mut self.monitors[idx];
        let kind = backend.kind();
        m.floor = model::floor(m.settings.min, DEFAULT_MIN, kind.hard_min());
        m.value = value.max(m.floor);
        m.status = Status::Active(backend);
        m.last_apply = None;
        m.refresh_column();
        if apply {
            self.apply(idx);
        }
        self.update_master();
    }

    fn set_monitor_value(&mut self, idx: usize, value: u8, update_slider: bool) {
        let m = &mut self.monitors[idx];
        if !matches!(m.status, Status::Active(_)) {
            return;
        }
        m.value = value.clamp(m.floor, 100);
        if update_slider {
            m.column.set_value(m.value);
        } else {
            m.column.value.set_label(&format!("{}%", m.value));
        }
        self.apply(idx);
        self.schedule_save();
        self.update_master();
    }

    /// Master slider moved: same absolute value on every monitor (spec §2.3).
    fn set_all(&mut self, value: u8) {
        for idx in 0..self.monitors.len() {
            let m = &mut self.monitors[idx];
            if !matches!(m.status, Status::Active(_)) {
                continue;
            }
            m.value = value.clamp(m.floor, 100);
            m.column.set_value(m.value);
            self.apply(idx);
        }
        self.master.scale.remove_css_class("indeterminate");
        self.master.value.set_label(&format!("{value}%"));
        self.schedule_save();
    }

    /// Show the average on the master slider; dim its thumb when monitors differ.
    fn update_master(&mut self) {
        let active: Vec<&Monitor> = self.monitors.iter().filter(|m| m.kind().is_some()).collect();
        let values: Vec<u8> = active.iter().map(|m| m.value).collect();
        let Some((avg, uniform)) = model::master_display(&values) else {
            self.master.scale.set_sensitive(false);
            self.master.value.set_label("–");
            return;
        };
        let floor = active.iter().map(|m| m.floor).min().unwrap_or(0);
        self.master.scale.set_sensitive(true);
        self.master.set_floor(floor);
        self.master.set_value(avg);
        if uniform {
            self.master.scale.remove_css_class("indeterminate");
        } else {
            self.master.scale.add_css_class("indeterminate");
        }
    }

    /// Send the monitor's value to its backend, at most once per THROTTLE.
    fn apply(&mut self, idx: usize) {
        let m = &mut self.monitors[idx];
        let Status::Active(backend) = &mut m.status else { return };
        // DDC workers coalesce on their own.
        let throttled = backend.kind() != Kind::Ddc;
        let now = Instant::now();
        if throttled
            && let Some(last) = m.last_apply.filter(|t| now - *t < THROTTLE) {
                if !m.apply_scheduled {
                    m.apply_scheduled = true;
                    let (weak, id) = (self.weak.clone(), m.id);
                    glib::timeout_add_local_once(THROTTLE - (now - last), move || {
                        with_app(&weak, |app| {
                            if let Some(idx) = app.index_of(id) {
                                app.monitors[idx].apply_scheduled = false;
                                app.monitors[idx].last_apply = None;
                                app.apply(idx);
                            }
                        });
                    });
                }
                return;
            }
        m.last_apply = Some(now);
        if let Err(e) = backend.set(m.value) {
            let msg = format!("{}: {e}", m.label());
            self.toast(&msg);
        }
    }

    fn on_ddc_event(&mut self, ev: DdcEvent) {
        match ev {
            DdcEvent::Ready { id, current, bus } => {
                let Some(idx) = self.index_of(id) else { return };
                let m = &mut self.monitors[idx];
                let Status::Probing(handle) = std::mem::replace(&mut m.status, Status::Unsupported) else { return };
                m.detail = Some(bus.display().to_string());
                m.note = None;
                self.activate(idx, Box::new(handle), current, false);
            }
            DdcEvent::Unavailable { id, reason } => {
                let Some(idx) = self.index_of(id) else { return };
                let m = &mut self.monitors[idx];
                if !matches!(m.status, Status::Probing(_)) {
                    return;
                }
                m.status = Status::Unsupported;
                m.note = Some(format!("DDC/CI unavailable: {reason}"));
                if m.settings.backend == BackendPref::Ddc {
                    let msg = format!("{}: DDC/CI unavailable ({reason}); using software dimming", m.label());
                    self.toast(&msg);
                }
                self.use_software(idx, None);
            }
            DdcEvent::Failed { id, reason } => {
                let Some(idx) = self.index_of(id) else { return };
                let m = &mut self.monitors[idx];
                if m.kind() != Some(Kind::Ddc) {
                    return;
                }
                let value = m.value;
                m.status = Status::Unsupported;
                m.note = Some(format!("DDC/CI failed: {reason}"));
                let msg = format!("{} stopped responding to DDC; switched to software dimming", m.label());
                self.toast(&msg);
                self.use_software(idx, Some(value));
            }
        }
    }

    fn on_gamma_failed(&mut self, connector: &str) {
        let Some(idx) = self.monitors.iter().position(|m| m.info.connector == connector) else { return };
        if self.monitors[idx].kind() != Some(Kind::Gamma) {
            return;
        }
        let value = self.monitors[idx].value;
        self.monitors[idx].note = Some("Gamma is in use by another app (e.g. gammastep or wlsunset); using overlay".into());
        let msg = format!("{}: gamma is in use by another app, using overlay instead", self.monitors[idx].label());
        self.toast(&msg);
        // is_failed() now reports true, so use_software picks the overlay.
        self.use_software(idx, Some(value));
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    fn schedule_save(&mut self) {
        for m in self.monitors.iter().filter(|m| m.kind().is_some()) {
            self.state.brightness.insert(m.info.connector.clone(), m.value);
        }
        if self.save_scheduled {
            return;
        }
        self.save_scheduled = true;
        let weak = self.weak.clone();
        glib::timeout_add_local_once(SAVE_DELAY, move || with_app(&weak, |app| app.save_now()));
    }

    fn save_now(&mut self) {
        self.save_scheduled = false;
        self.state.save();
    }
}

impl Controller for App {
    fn rows(&self) -> Vec<Row> {
        self.monitors
            .iter()
            .map(|m| Row {
                connector: m.info.connector.clone(),
                label: m.label(),
                description: m.info.pretty_name(),
                backend: m.badge_text().to_string(),
                value: m.kind().map(|_| m.value),
                detail: m.detail.clone(),
                note: m.note.clone(),
            })
            .collect()
    }

    fn set(&mut self, connector: &str, target: Target) -> Result<u8, String> {
        let idx = self.monitors.iter().position(|m| m.info.connector == connector).ok_or("no such monitor")?;
        let m = &self.monitors[idx];
        match m.status {
            Status::Active(_) => {}
            Status::Probing(_) => return Err("still probing DDC/CI, try again".into()),
            Status::Unsupported => return Err(m.note.clone().unwrap_or("not controllable".into())),
        }
        let value = target.resolve(m.value, m.floor);
        self.set_monitor_value(idx, value, true);
        Ok(self.monitors[idx].value)
    }
}

/// Run `f` on the app unless it's already borrowed. A busy borrow means the
/// event came from our own programmatic widget update, which must be ignored.
fn with_app(weak: &Weak<RefCell<App>>, f: impl FnOnce(&mut App)) {
    if let Some(app) = weak.upgrade()
        && let Ok(mut app) = app.try_borrow_mut() {
            f(&mut app);
        }
}
