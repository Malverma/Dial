//! Spectrum meter for the audio output that's playing: `parec` records that
//! sink's monitor, an FFT splits it into log-spaced bands, and a DrawingArea
//! draws them as gradient bars.

use std::f32::consts::PI;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{cairo, glib};

pub const BARS: usize = 48;
const RATE: usize = 48_000;
const FFT: usize = 4096;
/// New samples per analysis, ~60 per second.
const HOP: usize = 800;
const LOW_HZ: f32 = 50.0;
const HIGH_HZ: f32 = 16_000.0;
/// Levels shown as an empty and a full bar. Music spreads its energy over many
/// FFT bins, so a single bin rarely gets near full scale.
const FLOOR_DB: f32 = -78.0;
const CEIL_DB: f32 = -22.0;
/// Pink-noise tilt so music doesn't pile up on the left.
const TILT_DB_PER_OCTAVE: f32 = 3.0;
/// Full bar height per second a bar falls.
const DECAY: f32 = 1.6;
pub const HEIGHT: i32 = 48;

/// Background capture thread. It records only while running.
struct Capture {
    levels: Arc<Mutex<Vec<f32>>>,
    child: Arc<Mutex<Option<Child>>>,
    running: Arc<AtomicBool>,
    /// Source to record; the default output's monitor when unset.
    device: Arc<Mutex<Option<String>>>,
    thread: JoinHandle<()>,
}

impl Capture {
    fn spawn() -> Self {
        let levels = Arc::new(Mutex::new(vec![0.0; BARS]));
        let child: Arc<Mutex<Option<Child>>> = Arc::default();
        let running = Arc::new(AtomicBool::new(false));
        let device: Arc<Mutex<Option<String>>> = Arc::default();
        let (l, c, r, d) = (levels.clone(), child.clone(), running.clone(), device.clone());
        let thread = std::thread::spawn(move || capture_loop(&l, &c, &r, &d));
        Self { levels, child, running, device, thread }
    }

    fn set_running(&self, on: bool) {
        self.running.store(on, Ordering::SeqCst);
        if on {
            self.thread.thread().unpark();
        } else {
            self.kill();
            self.levels.lock().unwrap().fill(0.0);
        }
    }

    fn set_device(&self, device: Option<String>) {
        let mut d = self.device.lock().unwrap();
        if *d != device {
            *d = device;
            drop(d);
            // The thread reconnects to the new device.
            self.kill();
        }
    }

    fn kill(&self) {
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

fn capture_loop(
    levels: &Mutex<Vec<f32>>,
    slot: &Mutex<Option<Child>>,
    running: &AtomicBool,
    device: &Mutex<Option<String>>,
) {
    let window: Vec<f32> = (0..FFT).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / FFT as f32).cos()).collect();
    let bands = band_bins();
    let mut samples = vec![0.0f32; FFT];
    let mut buf = vec![0u8; HOP * 4];
    loop {
        if !running.load(Ordering::SeqCst) {
            std::thread::park();
            continue;
        }
        let spawned = Command::new("parec")
            .arg(format!("--device={}", device.lock().unwrap().as_deref().unwrap_or("@DEFAULT_MONITOR@")))
            .args(["--raw", "--format=float32le", "--channels=1"])
            .arg(format!("--rate={RATE}"))
            .args(["--latency-msec=15", "--stream-name=Spectrum meter"])
            .arg(format!("--client-name={}", crate::audio::OWN_CLIENT))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = spawned else { return }; // parec isn't installed
        let mut stdout = child.stdout.take().expect("piped stdout");
        *slot.lock().unwrap() = Some(child);
        if !running.load(Ordering::SeqCst) {
            // Stopped between the check and storing the child.
            kill_slot(slot);
            continue;
        }

        let mut got_any = false;
        while stdout.read_exact(&mut buf).is_ok() {
            got_any = true;
            samples.copy_within(HOP.., 0);
            for (s, b) in samples[FFT - HOP..].iter_mut().zip(buf.chunks_exact(4)) {
                *s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
            let out = spectrum(&samples, &window, &bands);
            levels.lock().unwrap().copy_from_slice(&out);
        }
        kill_slot(slot);
        samples.fill(0.0);
        levels.lock().unwrap().fill(0.0);
        if !got_any {
            // No server or no default output; don't spin.
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn kill_slot(slot: &Mutex<Option<Child>>) {
    if let Some(mut c) = slot.lock().unwrap().take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// FFT bin range `[lo, hi)` and centre frequency for each bar.
fn band_bins() -> Vec<(usize, usize, f32)> {
    let ratio = (HIGH_HZ / LOW_HZ).powf(1.0 / BARS as f32);
    let hz_per_bin = RATE as f32 / FFT as f32;
    (0..BARS)
        .map(|i| {
            let lo_hz = LOW_HZ * ratio.powi(i as i32);
            let hi_hz = lo_hz * ratio;
            let lo = (lo_hz / hz_per_bin).round() as usize;
            let hi = ((hi_hz / hz_per_bin).round() as usize).max(lo + 1);
            (lo, hi, (lo_hz * hi_hz).sqrt())
        })
        .collect()
}

/// Bar levels in 0..=1 for one block of samples.
fn spectrum(samples: &[f32], window: &[f32], bands: &[(usize, usize, f32)]) -> Vec<f32> {
    let mut re: Vec<f32> = samples.iter().zip(window).map(|(s, w)| s * w).collect();
    let mut im = vec![0.0; re.len()];
    fft(&mut re, &mut im);
    // A full-scale sine through a Hann window peaks at N/4.
    let scale = 4.0 / FFT as f32;
    bands
        .iter()
        .map(|&(lo, hi, centre)| {
            let mag = (lo..hi).map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt()).fold(0.0, f32::max) * scale;
            let db = 20.0 * (mag + 1e-9).log10() + TILT_DB_PER_OCTAVE * (centre / 1000.0).log2();
            ((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0)
        })
        .collect()
}

/// In-place iterative radix-2 FFT; the length must be a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        for start in (0..n).step_by(len) {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                (cr, ci) = (cr * wr - ci * wi, cr * wi + ci * wr);
            }
        }
        len <<= 1;
    }
}

/// The meter widget. Capture runs only while it is mapped.
pub struct Meter {
    pub area: gtk::DrawingArea,
    capture: Arc<Capture>,
}

impl Meter {
    pub fn new() -> Self {
        let capture = Arc::new(Capture::spawn());
        let area = gtk::DrawingArea::new();
        // Minimum height; the window's grid gives it half a slider row.
        area.set_content_height(HEIGHT);
        area.set_vexpand(true);
        area.set_hexpand(true);
        area.set_tooltip_text(Some("Spectrum of the output that's playing"));

        let shown = std::rc::Rc::new(std::cell::RefCell::new(vec![0.0f32; BARS]));
        let s = shown.clone();
        area.set_draw_func(move |_, cr, w, h| {
            let _ = draw(cr, w as f64, h as f64, &s.borrow());
        });

        let (c, s) = (capture.clone(), shown.clone());
        let last = std::cell::Cell::new(0i64);
        area.add_tick_callback(move |area, clock| {
            let now = clock.frame_time();
            let dt = if last.get() == 0 { 0.0 } else { (now - last.get()) as f32 / 1e6 };
            last.set(now);
            let target = c.levels.lock().unwrap().clone();
            let mut shown = s.borrow_mut();
            for (v, t) in shown.iter_mut().zip(target) {
                // Rise instantly, fall smoothly.
                *v = t.max(*v - DECAY * dt);
            }
            area.queue_draw();
            glib::ControlFlow::Continue
        });

        let c = capture.clone();
        area.connect_map(move |_| c.set_running(true));
        let c = capture.clone();
        area.connect_unmap(move |_| c.set_running(false));

        Self { area, capture }
    }

    /// Record this source (a sink's monitor) instead.
    pub fn set_device(&self, device: Option<String>) {
        self.capture.set_device(device);
    }
}

fn draw(cr: &cairo::Context, w: f64, h: f64, levels: &[f32]) -> Result<(), cairo::Error> {
    let r = 6.0;
    cr.new_sub_path();
    cr.arc(w - r, r, r, -PI as f64 / 2.0, 0.0);
    cr.arc(w - r, h - r, r, 0.0, PI as f64 / 2.0);
    cr.arc(r, h - r, r, PI as f64 / 2.0, PI as f64);
    cr.arc(r, r, r, PI as f64, 1.5 * PI as f64);
    cr.close_path();
    cr.set_source_rgba(0.09, 0.10, 0.15, 0.6);
    cr.fill()?;

    let pad = 6.0;
    let (iw, ih) = (w - 2.0 * pad, h - 2.0 * pad);
    let bottom = pad + ih;
    let gap = 2.0;
    let bw = ((iw - gap * (BARS - 1) as f64) / BARS as f64).max(1.0);

    // One gradient over the full height, so only tall bars reach pink.
    let grad = cairo::LinearGradient::new(0.0, bottom, 0.0, pad);
    grad.add_color_stop_rgb(0.0, 0.27, 0.88, 0.77);
    grad.add_color_stop_rgb(0.45, 0.28, 0.68, 0.94);
    grad.add_color_stop_rgb(0.75, 0.62, 0.55, 0.90);
    grad.add_color_stop_rgb(1.0, 0.93, 0.47, 0.77);
    for (i, &l) in levels.iter().enumerate() {
        let bh = (l as f64 * ih).max(2.0);
        cr.rectangle(pad + i as f64 * (bw + gap), bottom - bh, bw, bh);
    }
    cr.set_source(&grad)?;
    cr.fill()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_finds_a_sine() {
        let n = 64;
        let mut re: Vec<f32> = (0..n).map(|i| (2.0 * PI * 5.0 * i as f32 / n as f32).sin()).collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        let mags: Vec<f32> = re.iter().zip(&im).map(|(r, i)| (r * r + i * i).sqrt()).collect();
        assert!((mags[5] - n as f32 / 2.0).abs() < 1e-3);
        assert!(mags[3] < 1e-3 && mags[7] < 1e-3);
    }

    #[test]
    fn loud_tone_fills_its_bar_only() {
        let window: Vec<f32> = (0..FFT).map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / FFT as f32).cos()).collect();
        let bands = band_bins();
        let hz = 1000.0;
        let samples: Vec<f32> = (0..FFT).map(|i| (2.0 * PI * hz * i as f32 / RATE as f32).sin()).collect();
        let out = spectrum(&samples, &window, &bands);
        let peak = out.iter().cloned().fold(0.0, f32::max);
        assert!(peak > 0.95, "peak {peak}");
        assert!(out[0] < 0.2 && out[BARS - 1] < 0.2);
        assert!(spectrum(&vec![0.0; FFT], &window, &bands).iter().all(|&v| v == 0.0));
    }
}
