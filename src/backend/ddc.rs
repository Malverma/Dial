//! External monitor brightness over DDC/CI (VCP feature 0x10).
//!
//! DDC is slow (tens of ms per command), so the GUI talks to each monitor
//! through a worker thread that only ever sends the most recent value.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use ddc::{Ddc, Edid};

use super::{Backend, Kind};
use crate::{curve, discovery};

const VCP_LUMINANCE: u8 = 0x10;
const RETRIES: usize = 3;
const MIN_WRITE_INTERVAL: Duration = Duration::from_millis(100);

pub struct DdcDevice {
    bus: PathBuf,
    ddc: ddc_i2c::I2cDeviceDdc,
    max: u16,
    gamma: f64,
}

impl DdcDevice {
    /// Finds the monitor's bus, opens it and reads the luminance range.
    pub fn open(connector: &str, gamma: f64) -> Result<Self, String> {
        let bus = find_bus(connector)?;
        let mut ddc = ddc_i2c::from_i2c_device(&bus).map_err(|e| format!("{}: {e}", bus.display()))?;
        let v = retry(|| ddc.get_vcp_feature(VCP_LUMINANCE).map_err(|e| e.to_string()))
            .map_err(|e| format!("monitor does not answer DDC/CI on {}: {e}", bus.display()))?;
        let max = u16::from_be_bytes([v.mh, v.ml]);
        Ok(Self { bus, ddc, max: if max == 0 { 100 } else { max }, gamma })
    }

    pub fn bus(&self) -> &Path {
        &self.bus
    }

    pub fn get(&mut self) -> Result<u8, String> {
        let v = retry(|| self.ddc.get_vcp_feature(VCP_LUMINANCE).map_err(|e| e.to_string()))?;
        Ok(curve::from_native(v.value() as u32, self.max as u32, self.gamma))
    }

    pub fn set(&mut self, pct: u8) -> Result<(), String> {
        let value = curve::to_native(pct, self.max as u32, self.gamma) as u16;
        retry(|| self.ddc.set_vcp_feature(VCP_LUMINANCE, value).map_err(|e| e.to_string()))
    }
}

fn retry<T>(mut f: impl FnMut() -> Result<T, String>) -> Result<T, String> {
    let mut last = String::new();
    for attempt in 0..RETRIES {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
        if attempt + 1 < RETRIES {
            thread::sleep(Duration::from_millis(50));
        }
    }
    Err(last)
}

/// EDID (first 128 bytes) of every display-capable bus, scanned once and shared.
type BusEdids = Vec<(PathBuf, Vec<u8>)>;
static BUS_CACHE: Mutex<Option<BusEdids>> = Mutex::new(None);

/// Forget cached bus ↔ EDID pairs, e.g. after a monitor was plugged in.
pub fn invalidate_bus_cache() {
    *BUS_CACHE.lock().unwrap() = None;
}

fn find_bus(connector: &str) -> Result<PathBuf, String> {
    if !discovery::i2c_dev_loaded() {
        return Err("i2c-dev kernel module not loaded".into());
    }
    if let Some(bus) = discovery::i2c_for(connector) {
        return Ok(bus);
    }
    // Proprietary NVIDIA doesn't link connectors to buses: match by EDID instead.
    let want = discovery::edid_bytes(connector).ok_or("no EDID for connector")?;
    let want = &want[..want.len().min(128)];
    let mut cache = BUS_CACHE.lock().unwrap();
    if cache.is_none() {
        *cache = Some(scan_buses()?);
    }
    cache
        .as_ref()
        .unwrap()
        .iter()
        .find(|(_, edid)| edid.as_slice() == want)
        .map(|(bus, _)| bus.clone())
        .ok_or_else(|| "no I²C bus answers with this monitor's EDID".into())
}

fn scan_buses() -> Result<BusEdids, String> {
    let candidates = discovery::display_i2c_buses();
    let mut opened = 0;
    let mut found = Vec::new();
    for bus in candidates.iter() {
        let Ok(mut dev) = ddc_i2c::from_i2c_device(bus) else { continue };
        opened += 1;
        let mut buf = [0u8; 128];
        if dev.read_edid(0, &mut buf).ok() == Some(128) {
            found.push((bus.clone(), buf.to_vec()));
        }
    }
    if opened == 0 && !candidates.is_empty() {
        return Err("no permission to open /dev/i2c-* (see README: DDC/CI setup)".into());
    }
    Ok(found)
}

pub enum DdcEvent {
    Ready { id: u64, current: u8, bus: PathBuf },
    Unavailable { id: u64, reason: String },
    Failed { id: u64, reason: String },
}

#[derive(Default)]
struct SlotState {
    pending: Option<u8>,
    quit: bool,
}

#[derive(Default)]
struct Slot {
    state: Mutex<SlotState>,
    cv: Condvar,
}

/// GUI-side handle to a monitor's DDC worker. Dropping it stops the worker.
pub struct DdcHandle {
    slot: Arc<Slot>,
}

impl DdcHandle {
    /// Starts a worker that probes the monitor and reports back on `tx`.
    pub fn spawn(id: u64, connector: String, gamma: f64, tx: async_channel::Sender<DdcEvent>) -> Self {
        let slot = Arc::new(Slot::default());
        let worker_slot = slot.clone();
        thread::Builder::new()
            .name(format!("ddc-{connector}"))
            .spawn(move || worker(id, &connector, gamma, &worker_slot, &tx))
            .expect("spawn DDC worker");
        Self { slot }
    }
}

impl Drop for DdcHandle {
    fn drop(&mut self) {
        self.slot.state.lock().unwrap().quit = true;
        self.slot.cv.notify_one();
    }
}

impl Backend for DdcHandle {
    fn kind(&self) -> Kind {
        Kind::Ddc
    }

    fn set(&mut self, pct: u8) -> Result<(), String> {
        self.slot.state.lock().unwrap().pending = Some(pct);
        self.slot.cv.notify_one();
        Ok(())
    }
}

fn worker(id: u64, connector: &str, gamma: f64, slot: &Slot, tx: &async_channel::Sender<DdcEvent>) {
    let mut dev = match DdcDevice::open(connector, gamma).and_then(|mut d| d.get().map(|v| (d, v))) {
        Ok((dev, current)) => {
            let _ = tx.send_blocking(DdcEvent::Ready { id, current, bus: dev.bus.clone() });
            dev
        }
        Err(reason) => {
            let _ = tx.send_blocking(DdcEvent::Unavailable { id, reason });
            return;
        }
    };

    loop {
        let pct = {
            let mut st = slot.state.lock().unwrap();
            while st.pending.is_none() && !st.quit {
                st = slot.cv.wait(st).unwrap();
            }
            if st.quit {
                return;
            }
            st.pending.take().unwrap()
        };
        if let Err(reason) = dev.set(pct) {
            let _ = tx.send_blocking(DdcEvent::Failed { id, reason });
            return;
        }
        thread::sleep(MIN_WRITE_INTERVAL);
    }
}
