//! The GTK window: the brightness slider for all monitors next to one volume
//! slider per application, a row of device volumes (microphone plus one per
//! output) and a spectrum meter.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::audio::{self, Snapshot};
use crate::backend::ddc::{self, DdcEvent, DdcHandle};
use crate::backend::gamma::GammaManager;
use crate::backend::{backlight::Backlight, overlay, overlay::Overlay, Backend, Kind};
use crate::cli::{Controller, Row};
use crate::config::{BackendPref, Config, MonitorSettings, State, DEFAULT_MIN};
use crate::discovery::{self, MonitorInfo};
use crate::meter::Meter;
use crate::model::{self, Target};

const THROTTLE: Duration = Duration::from_millis(16);
const SAVE_DELAY: Duration = Duration::from_millis(500);
const HOTPLUG_DELAY: Duration = Duration::from_millis(300);
const MIC_MAX: u8 = 200;
const OUTPUT_MAX: u8 = 100;
const APP_MAX: u8 = 100;
/// An app's slider outlives its last stream by this long, so it doesn't
/// flicker away between songs.
const APP_GRACE: Duration = Duration::from_secs(5);
/// While dragging an unlocked app's slider, ignore its own echoes.
const ECHO: Duration = Duration::from_millis(400);
/// Slider trough height. A row is at least this plus its labels, so the
/// meter stays under half a row.
const SLIDER_HEIGHT: i32 = 40;
/// Side of the square the window becomes when floated in Hyprland.
const FLOAT_SIZE: u32 = 480;

const CSS: &str = "
.dial-overlay { background: black; }

/* Transparent main window; the compositor's blur (if any) shows through. */
window.dial-main {
    --window-bg-color: transparent;
    --headerbar-bg-color: transparent;
    --headerbar-backdrop-color: transparent;
    --headerbar-shade-color: transparent;
    background: transparent;
}
window.dial-main headerbar { background: transparent; box-shadow: none; }

/* Rectangular slider thumbs, wide across the vertical trough. */
scale.vertical slider {
    min-width: 30px;
    min-height: 12px;
    margin: -6px -13px;
    border-radius: 3px;
}
scale.indeterminate slider { opacity: 0.45; }
.column-name { font-weight: bold; }
/* The rows get identical padding and border widths so their sliders stay the
   same length; the line between them is the top row's bottom border. */
.brightness-row { border-bottom: 1px solid alpha(currentColor, 0.15); padding-bottom: 12px; }
.audio-row { border-top: 1px solid transparent; padding-top: 12px; }
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
    fn new(title: &str, step: u8, max: u8) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
        root.set_width_request(84);

        let name = gtk::Label::new(Some(title));
        name.add_css_class("column-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(12);

        let scale = gtk::Scale::with_range(gtk::Orientation::Vertical, 0.0, max as f64, 1.0);
        scale.set_inverted(true);
        scale.set_draw_value(false);
        scale.set_round_digits(0);
        scale.set_vexpand(true);
        scale.set_halign(gtk::Align::Center);
        scale.set_size_request(-1, SLIDER_HEIGHT);
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
                gdk::Key::Home | gdk::Key::KP_Home => s.set_value(s.adjustment().upper()),
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
                s.set_value(v.clamp(0.0, s.adjustment().upper()));
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

/// One audio output. Its volume is locked to `State::volume`: changes made
/// elsewhere (players, Bluetooth resets) are reverted.
struct Output {
    name: String,
    column: Column,
}

/// Every stream of one application (all its instances) under one slider.
struct AppAudio {
    app: String,
    column: Column,
    /// When its last stream closed.
    gone_since: Option<Instant>,
    /// Last time the user moved its slider.
    touched: Option<Instant>,
}

pub struct App {
    weak: Weak<RefCell<App>>,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    /// Shows the app sliders, or a placeholder when nothing is playing.
    stack: gtk::Stack,
    apps_box: gtk::Box,
    apps: Vec<AppAudio>,
    master: Column,
    monitors: Vec<Monitor>,
    last_audio: Option<Snapshot>,
    outputs_box: gtk::Box,
    outputs: Vec<Output>,
    mic: Column,
    /// The default source the mic slider controls.
    mic_device: Option<String>,
    volumes: audio::Setter,
    meter: Meter,
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
        let mut state = State::load();
        if !config.general.restore_on_start {
            // Lock outputs to whatever they're at now instead.
            state.volume.clear();
            state.app_volume.clear();
        }
        let gamma = GammaManager::connect();

        let master = Column::new("DIM", config.general.scroll_step, 100);
        master.badge.set_label("All monitors");
        master.value.set_tooltip_text(None);

        // Columns share the leftover width equally so the sliders fill a tiled window.
        let apps_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        apps_box.set_homogeneous(true);
        apps_box.set_hexpand(true);

        let empty = gtk::Label::new(Some("No apps are playing audio"));
        empty.add_css_class("dim-label");

        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.add_named(&apps_box, Some("content"));
        stack.add_named(&empty, Some("empty"));
        stack.set_visible_child_name("empty");

        let top_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        top_row.set_vexpand(true);
        top_row.append(&master.root);
        top_row.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        top_row.append(&stack);

        let mic = Column::new("MIC", config.general.scroll_step, MIC_MAX);
        mic.badge.set_label("Mic gain");
        mic.value.set_tooltip_text(None);
        mic.scale.set_sensitive(false);
        mic.value_stack.set_sensitive(false);
        let outputs_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        outputs_box.set_homogeneous(true);
        outputs_box.set_hexpand(true);
        let volume_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        volume_row.set_vexpand(true);
        volume_row.append(&mic.root);
        volume_row.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        volume_row.append(&outputs_box);

        let meter = Meter::new();

        // Each slider row spans two equal grid rows and the meter one, so the
        // meter grows with the window but stays half a row tall.
        top_row.add_css_class("brightness-row");
        volume_row.add_css_class("audio-row");
        meter.area.set_margin_top(12);
        let content = gtk::Grid::new();
        content.set_row_homogeneous(true);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        content.attach(&top_row, 0, 0, 1, 2);
        content.attach(&volume_row, 0, 2, 1, 2);
        content.attach(&meter.area, 0, 4, 1, 1);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&content));

        let settings = gtk::MenuButton::new();
        settings.set_icon_name("emblem-system-symbolic");
        settings.set_tooltip_text(Some("Settings"));
        let header = adw::HeaderBar::new();
        // Closing lives in the settings popover.
        header.set_show_end_title_buttons(false);
        header.pack_end(&settings);

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&toasts));

        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Dial")
            // Resizable so Hyprland tiles it; it floats windows whose min and max size match.
            .default_width(480)
            .default_height(480)
            .content(&view)
            .build();
        window.add_css_class("dial-main");
        crate::hypr::float_as_square(FLOAT_SIZE);

        let (ddc_tx, ddc_rx) = async_channel::unbounded();

        let app = Rc::new_cyclic(|weak| {
            RefCell::new(App {
                weak: weak.clone(),
                window: window.clone(),
                toasts,
                stack,
                apps_box,
                apps: Vec::new(),
                master,
                monitors: Vec::new(),
                last_audio: None,
                outputs_box,
                outputs: Vec::new(),
                mic,
                mic_device: None,
                volumes: audio::Setter::spawn(),
                meter,
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
            let weak = Rc::downgrade(&app);
            a.mic.scale.connect_value_changed(move |s| {
                with_app(&weak, |app| app.set_mic(s.value() as u8));
            });
        }

        let weak = Rc::downgrade(&app);
        settings.set_create_popup_func(move |button| {
            let (mut entries, mut show_silent) = (Vec::new(), true);
            with_app(&weak, |app| {
                entries = app.lock_entries();
                show_silent = !app.state.hide_silent_apps;
            });
            button.set_popover(Some(&settings_popover(&weak, entries, show_silent)));
        });

        let (audio_tx, audio_rx) = async_channel::unbounded();
        audio::watch(audio_tx);
        let weak = Rc::downgrade(&app);
        glib::spawn_future_local(async move {
            while let Ok(snapshot) = audio_rx.recv().await {
                let Some(app) = weak.upgrade() else { break };
                app.borrow_mut().on_audio(snapshot);
            }
        });

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

        self.monitors.retain(|m| current.iter().any(|(i, g)| i.connector == m.info.connector && *g == m.gdk));

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
        self.update_master();
    }

    fn add_monitor(&mut self, info: MonitorInfo, gdk_monitor: gdk::Monitor) {
        let id = self.next_id;
        self.next_id += 1;
        let settings = self.config.settings_for(&info);
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
        self.update_master();
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
            self.update_master();
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
        if apply {
            self.apply(idx);
        }
        self.update_master();
    }

    fn set_monitor_value(&mut self, idx: usize, value: u8) {
        let m = &mut self.monitors[idx];
        if !matches!(m.status, Status::Active(_)) {
            return;
        }
        m.value = value.clamp(m.floor, 100);
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
            self.apply(idx);
        }
        self.master.scale.remove_css_class("indeterminate");
        self.master.value.set_label(&format!("{value}%"));
        self.update_master_badge();
        self.schedule_save();
    }

    /// Show the average on the master slider; dim its thumb when monitors differ.
    fn update_master(&mut self) {
        self.update_master_badge();
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

    fn update_master_badge(&self) {
        self.master.badge.set_label(if self.monitors.is_empty() { "No monitors" } else { "All monitors" });
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

    /// Sync output columns and the mic with the audio server, and put back any
    /// volume that something else changed.
    fn on_audio(&mut self, snapshot: Option<Snapshot>) {
        self.last_audio = snapshot.clone();
        let snapshot = snapshot.unwrap_or_default();
        self.remember_apps(&snapshot);
        self.sync_apps(&snapshot);

        let outputs_box = &self.outputs_box;
        self.outputs.retain(|o| {
            let keep = snapshot.sinks.iter().any(|d| d.name == o.name);
            if !keep {
                outputs_box.remove(&o.column.root);
            }
            keep
        });

        for d in &snapshot.sinks {
            let target = self.locked_volume(d, OUTPUT_MAX);
            let idx = match self.outputs.iter().position(|o| o.name == d.name) {
                Some(idx) => idx,
                None => {
                    self.add_output(&d.name, &d.description, target);
                    self.outputs.len() - 1
                }
            };
            let default = snapshot.default_sink.as_deref() == Some(d.name.as_str());
            self.outputs[idx].column.badge.set_label(if default { "Default" } else { "Output" });
            if d.volume != target {
                self.volumes.set(audio::Kind::Sink, &d.name, target);
            }
        }
        for (i, o) in self.outputs.iter().enumerate() {
            let prev = i.checked_sub(1).map(|p| self.outputs[p].column.root.clone());
            self.outputs_box.reorder_child_after(&o.column.root, prev.as_ref());
        }

        match &snapshot.source {
            Some(d) => {
                let target = self.locked_volume(d, MIC_MAX);
                if self.mic_device.as_deref() != Some(d.name.as_str()) {
                    self.mic_device = Some(d.name.clone());
                    self.mic.scale.set_sensitive(true);
                    self.mic.value_stack.set_sensitive(true);
                    self.mic.set_value(target);
                }
                // An unlocked app that's recording (e.g. Discord's automatic
                // gain) may change the gain; follow it instead of fighting.
                let free = snapshot.recording.iter().find(|a| !self.state.app_locked(a));
                if free.is_some() {
                    let v = d.volume.min(MIC_MAX);
                    if v != target {
                        self.state.volume.insert(d.name.clone(), v);
                        self.mic.set_value(v);
                        self.schedule_save();
                    }
                } else if d.volume != target {
                    self.volumes.set(audio::Kind::Source, &d.name, target);
                }
                self.mic.badge.set_label(if free.is_some() { "Mic gain (free)" } else { "Mic gain" });
            }
            None => {
                self.mic_device = None;
                self.mic.scale.set_sensitive(false);
                self.mic.value_stack.set_sensitive(false);
                self.mic.value.set_label("–");
            }
        }

        // Meter whatever is playing: the default output if it's in use, else
        // the first output that is.
        let default = snapshot.sinks.iter().find(|d| Some(&d.name) == snapshot.default_sink.as_ref());
        let playing = default.filter(|d| d.running).or_else(|| snapshot.sinks.iter().find(|d| d.running)).or(default);
        self.meter.set_device(playing.and_then(|d| d.monitor.clone()));
    }

    /// The volume a device is held at: the last one set here, else its current one.
    fn locked_volume(&mut self, d: &audio::Device, max: u8) -> u8 {
        *self.state.volume.entry(d.name.clone()).or_insert(d.volume.min(max))
    }

    fn add_output(&mut self, name: &str, description: &str, value: u8) {
        let column = Column::new(description, self.config.general.scroll_step, OUTPUT_MAX);
        column.root.set_tooltip_text(Some(&format!("{description}\n{name}")));
        column.set_value(value);
        let (weak, n) = (self.weak.clone(), name.to_string());
        column.scale.connect_value_changed(move |s| {
            with_app(&weak, |app| app.set_output(&n, s.value() as u8));
        });
        self.outputs_box.append(&column.root);
        self.outputs.push(Output { name: name.to_string(), column });
    }

    fn set_output(&mut self, name: &str, value: u8) {
        let Some(o) = self.outputs.iter().find(|o| o.name == name) else { return };
        o.column.value.set_label(&format!("{value}%"));
        self.state.volume.insert(name.to_string(), value);
        self.volumes.set(audio::Kind::Sink, name, value);
        self.schedule_save();
    }

    fn set_mic(&mut self, value: u8) {
        let Some(name) = self.mic_device.clone() else { return };
        self.mic.value.set_label(&format!("{value}%"));
        self.state.volume.insert(name.clone(), value);
        self.volumes.set(audio::Kind::Source, &name, value);
        self.schedule_save();
    }

    /// One slider per application (every instance of it together), for apps
    /// playing audio and, unless hidden, apps with a window. Holds locked
    /// apps' streams at their volume.
    fn sync_apps(&mut self, snapshot: &Snapshot) {
        let now = Instant::now();
        let streams = &snapshot.streams;
        let mut apps: Vec<(&str, &str)> = Vec::new();
        let windowed = snapshot.windows.iter().filter(|_| !self.state.hide_silent_apps);
        for (app, name) in windowed.map(|(a, n)| (a.as_str(), n.as_str())).chain(streams.iter().map(|s| (s.app.as_str(), s.app_name.as_str()))) {
            if !apps.iter().any(|(a, _)| *a == app) {
                // Prefer the name the stream reports.
                let name = streams.iter().find(|s| s.app == app).map_or(name, |s| s.app_name.as_str());
                apps.push((app, name));
            }
        }

        for &(app, name) in &apps {
            let mine: Vec<&audio::Stream> = streams.iter().filter(|t| t.app == app).collect();
            let locked = self.state.app_locked(app);
            let current = mine.iter().map(|t| t.volume).max().unwrap_or(100).min(APP_MAX);
            let value = if locked || mine.is_empty() {
                *self.state.app_volume.entry(app.to_string()).or_insert(current)
            } else {
                current
            };

            let idx = match self.apps.iter().position(|a| a.app == app) {
                Some(idx) => idx,
                None => {
                    self.add_app(app, name, value);
                    self.apps.len() - 1
                }
            };
            let a = &mut self.apps[idx];
            a.gone_since = None;
            a.column.scale.set_sensitive(true);
            a.column.value_stack.set_sensitive(true);
            let n = mine.len();
            let (badge, tip) = match (n, locked) {
                (0, _) => ("No audio", format!("{name}\nNot playing audio; its streams start at this volume")),
                (_, true) => ("Locked", format!("{name} ({n} stream{})\nVolume is held here", if n == 1 { "" } else { "s" })),
                (_, false) => ("Unlocked", format!("{name} ({n} stream{})\nThe app may change its own volume", if n == 1 { "" } else { "s" })),
            };
            a.column.badge.set_label(badge);
            a.column.root.set_tooltip_text(Some(&tip));

            if locked || n == 0 {
                for t in mine.iter().filter(|t| t.volume != value) {
                    self.volumes.set(audio::Kind::SinkInput, &t.index.to_string(), value);
                }
            } else if a.touched.is_none_or(|t| now - t > ECHO) && a.column.scale.value() as u8 != value {
                // Follow the app.
                a.column.set_value(value);
                self.state.app_volume.insert(app.to_string(), value);
            }
        }
        let seen: Vec<&str> = apps.iter().map(|(a, _)| *a).collect();

        let mut pending = false;
        for a in &mut self.apps {
            if !seen.contains(&a.app.as_str()) && a.gone_since.is_none() {
                a.gone_since = Some(now);
                a.column.scale.set_sensitive(false);
                a.column.value_stack.set_sensitive(false);
                pending = true;
            }
        }
        if pending {
            let weak = self.weak.clone();
            glib::timeout_add_local_once(APP_GRACE + Duration::from_millis(50), move || {
                with_app(&weak, |app| app.prune_apps());
            });
        }
        self.stack.set_visible_child_name(if self.apps.is_empty() { "empty" } else { "content" });
    }

    fn prune_apps(&mut self) {
        let now = Instant::now();
        let apps_box = &self.apps_box;
        self.apps.retain(|a| {
            let keep = a.gone_since.is_none_or(|t| now - t < APP_GRACE);
            if !keep {
                apps_box.remove(&a.column.root);
            }
            keep
        });
        self.stack.set_visible_child_name(if self.apps.is_empty() { "empty" } else { "content" });
    }

    fn add_app(&mut self, app: &str, name: &str, value: u8) {
        let column = Column::new(name, self.config.general.scroll_step, APP_MAX);
        column.set_value(value);
        let (weak, a) = (self.weak.clone(), app.to_string());
        column.scale.connect_value_changed(move |s| {
            with_app(&weak, |app| app.set_app(&a, s.value() as u8));
        });
        self.apps_box.append(&column.root);
        self.apps.push(AppAudio { app: app.to_string(), column, gone_since: None, touched: None });
    }

    fn set_app(&mut self, app: &str, value: u8) {
        let Some(a) = self.apps.iter_mut().find(|a| a.app == app) else { return };
        a.column.value.set_label(&format!("{value}%"));
        a.touched = Some(Instant::now());
        self.state.app_volume.insert(app.to_string(), value);
        let streams = self.last_audio.as_ref().map(|s| s.streams.as_slice()).unwrap_or_default();
        for s in streams.iter().filter(|s| s.app == app) {
            self.volumes.set(audio::Kind::SinkInput, &s.index.to_string(), value);
        }
        self.schedule_save();
    }

    fn app_name(&self, app: &str) -> String {
        self.last_audio
            .iter()
            .flat_map(|s| &s.streams)
            .find(|s| s.app == app)
            .map(|s| s.app_name.clone())
            .or_else(|| self.state.known_apps.get(app).cloned())
            .unwrap_or_else(|| audio::title_case(app))
    }

    /// Apps for the settings list: open ones, ones with a saved choice, and
    /// the ones unlocked by default. `(id, name, locked)`.
    fn lock_entries(&self) -> Vec<(String, String, bool)> {
        let mut ids: Vec<String> = self.apps.iter().map(|a| a.app.clone()).collect();
        let remembered = self.state.known_apps.keys().chain(self.state.app_lock.keys()).map(String::as_str);
        for id in remembered.chain(crate::config::UNLOCKED_BY_DEFAULT.iter().copied()) {
            if !ids.iter().any(|i| i == id) {
                ids.push(id.to_string());
            }
        }
        let mut entries: Vec<_> = ids
            .into_iter()
            .map(|id| {
                let name = self.app_name(&id);
                let locked = self.state.app_locked(&id);
                (id, name, locked)
            })
            .collect();
        entries.sort_by_key(|(_, name, _)| name.to_lowercase());
        entries
    }

    /// Remember every app seen (with a window or playing audio), so its
    /// setting can be changed even when it isn't open.
    fn remember_apps(&mut self, snapshot: &Snapshot) {
        let seen = snapshot.windows.iter().cloned().chain(snapshot.streams.iter().map(|s| (s.app.clone(), s.app_name.clone())));
        let mut changed = false;
        for (id, name) in seen {
            if self.state.known_apps.get(&id) != Some(&name) {
                // Keep the name a stream reported over a window class.
                if self.state.known_apps.contains_key(&id) && !snapshot.streams.iter().any(|s| s.app == id) {
                    continue;
                }
                self.state.known_apps.insert(id, name);
                changed = true;
            }
        }
        if changed {
            self.schedule_save();
        }
    }

    fn set_app_locked(&mut self, app: &str, locked: bool) {
        self.state.app_lock.insert(app.to_string(), locked);
        if let Some(a) = self.apps.iter().find(|a| a.app == app) {
            // Hold it at what the slider shows now.
            self.state.app_volume.insert(app.to_string(), a.column.scale.value() as u8);
        }
        self.schedule_save();
        let snapshot = self.last_audio.clone();
        if snapshot.is_some() {
            self.on_audio(snapshot);
        }
    }

    fn set_show_silent(&mut self, show: bool) {
        self.state.hide_silent_apps = !show;
        self.schedule_save();
        let snapshot = self.last_audio.clone();
        if snapshot.is_some() {
            self.on_audio(snapshot);
        }
        // Hidden apps go now rather than after the grace period.
        if !show {
            let past = Instant::now().checked_sub(APP_GRACE);
            for a in self.apps.iter_mut().filter(|a| a.gone_since.is_some()) {
                a.gone_since = past;
            }
            self.prune_apps();
        }
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
        self.set_monitor_value(idx, value);
        Ok(self.monitors[idx].value)
    }
}

/// The settings popover: which apps get their volume held.
fn settings_popover(weak: &Weak<RefCell<App>>, entries: Vec<(String, String, bool)>, show_silent: bool) -> gtk::Popover {
    let general = gtk::ListBox::new();
    general.add_css_class("boxed-list");
    general.set_selection_mode(gtk::SelectionMode::None);
    let silent = adw::SwitchRow::builder()
        .title("Apps Without Audio")
        .subtitle("Show a slider for every open app")
        .active(show_silent)
        .build();
    let w = weak.clone();
    silent.connect_active_notify(move |r| with_app(&w, |app| app.set_show_silent(r.is_active())));
    general.append(&silent);

    let title = gtk::Label::new(Some("Hold App Volumes"));
    title.add_css_class("heading");
    title.set_halign(gtk::Align::Start);
    let hint = gtk::Label::new(Some(
        "Dial puts back volume changes it didn't make. Turn this off for apps that manage their own volume or mic gain.",
    ));
    hint.add_css_class("caption");
    hint.add_css_class("dim-label");
    hint.set_wrap(true);
    hint.set_max_width_chars(36);
    hint.set_xalign(0.0);

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    for (id, name, locked) in entries {
        let row = adw::SwitchRow::builder().title(name).active(locked).build();
        let weak = weak.clone();
        row.connect_active_notify(move |r| with_app(&weak, |app| app.set_app_locked(&id, r.is_active())));
        list.append(&row);
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);
    content.append(&general);
    content.append(&title);
    content.append(&hint);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_hscrollbar_policy(gtk::PolicyType::Never);
    scroll.set_propagate_natural_height(true);
    scroll.set_max_content_height(360);
    scroll.set_child(Some(&list));
    content.append(&scroll);

    let close = gtk::Button::with_label("Close Dial");
    close.add_css_class("destructive-action");
    let w = weak.clone();
    close.connect_clicked(move |_| {
        // Not borrowed here, so the close handler can save.
        let window = w.upgrade().map(|app| app.borrow().window.clone());
        if let Some(window) = window {
            window.close();
        }
    });
    content.append(&close);

    let popover = gtk::Popover::new();
    popover.set_child(Some(&content));
    popover
}

/// Run `f` on the app unless it's already borrowed. A busy borrow means the
/// event came from our own programmatic widget update, which must be ignored.
fn with_app(weak: &Weak<RefCell<App>>, f: impl FnOnce(&mut App)) {
    if let Some(app) = weak.upgrade()
        && let Ok(mut app) = app.try_borrow_mut() {
            f(&mut app);
        }
}
