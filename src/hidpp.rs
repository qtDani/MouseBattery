//! Logitech HID++ 2.0: find the mouse behind the receiver and read its battery.
//! SPDX-License-Identifier: GPL-3.0-only
//! Protocol logic ported to Rust (and modified, November 2026) from LGSTrayBattery by andyvorld,
//! https://github.com/andyvorld/LGSTrayBattery, licensed under GPL-3.0.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::hid::{self, HidDevice, ReadResult};

/// Our software id (low nibble of byte 3). Different from G HUB / LGSTray so replies never get mixed up.
const SW_ID: u8 = 0x0B;
const POLL_PERIOD: Duration = Duration::from_secs(60);
const RETRY_PERIOD: Duration = Duration::from_secs(5);
/// Device indexes behind a receiver (1-6), plus 0xFF for a mouse plugged in by cable.
const DEVICE_INDEXES: [u8; 7] = [1, 2, 3, 4, 5, 6, 0xFF];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Charge {
    Discharging,
    Charging,
    Full,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct BatteryState {
    /// A mouse with a battery feature has been found
    pub found: bool,
    /// The last request to the mouse succeeded (false = asleep / switched off)
    pub online: bool,
    pub name: String,
    /// -1 = not read yet
    pub percent: i32,
    pub charge: Charge,
    /// Local time (hour, minute) of the last successful reading
    pub updated: Option<(u16, u16)>,
}

impl Default for BatteryState {
    fn default() -> Self {
        BatteryState { found: false, online: false, name: String::new(), percent: -1, charge: Charge::Unknown, updated: None }
    }
}

pub struct Shared {
    pub state: Mutex<BatteryState>,
    pub notify: Box<dyn Fn() + Send + Sync>,
}

pub enum Msg {
    Report(u32, Vec<u8>),
    Gone(u32),
    Refresh,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BatteryKind {
    Level,   // 0x1000
    Voltage, // 0x1001
    Unified, // 0x1004
}

struct Device {
    idx: u8,
    feat: u8,
    kind: BatteryKind,
    name: String,
}

enum ReqErr {
    Timeout,
    Error,
    Gone,
}

struct Session {
    id: u32,
    short: Arc<HidDevice>,
    long: Arc<HidDevice>,
    stop: Arc<AtomicBool>,
}

impl Session {
    fn open(paths: &hid::HidppPaths, id: u32, tx: &Sender<Msg>) -> Option<Session> {
        let short = Arc::new(HidDevice::open(&paths.short)?);
        let long = Arc::new(HidDevice::open(&paths.long)?);
        let stop = Arc::new(AtomicBool::new(false));
        for dev in [short.clone(), long.clone()] {
            let tx = tx.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match dev.read(3000) {
                        ReadResult::Data(d) => {
                            if tx.send(Msg::Report(id, d)).is_err() {
                                break;
                            }
                        }
                        ReadResult::Timeout => {}
                        ReadResult::Gone => {
                            let _ = tx.send(Msg::Gone(id));
                            break;
                        }
                    }
                }
            });
        }
        Some(Session { id, short, long, stop })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct Ctx<'a> {
    rx: &'a Receiver<Msg>,
    sess: &'a Session,
    backlog: Vec<Vec<u8>>,
    refresh: bool,
    ping: u8,
}

impl<'a> Ctx<'a> {
    fn request(&mut self, idx: u8, feat: u8, func: u8, p: [u8; 3], timeout: Duration) -> Result<Vec<u8>, ReqErr> {
        let fsw = (func << 4) | SW_ID;
        let msg = [0x10, idx, feat, fsw, p[0], p[1], p[2]];
        if !self.sess.short.write(&msg) {
            // Some devices only take long reports
            let mut long = [0u8; 20];
            long[0] = 0x11;
            long[1..7].copy_from_slice(&msg[1..]);
            if !self.sess.long.write(&long) {
                return Err(ReqErr::Error);
            }
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ReqErr::Timeout);
            }
            match self.rx.recv_timeout(left) {
                Ok(Msg::Report(id, d)) if id == self.sess.id => {
                    if d.len() >= 7 && d[1] == idx {
                        // HID++ 1.0 error: [10, idx, 8F, feat, func|sw, code, 0]
                        if d[2] == 0x8F && d[3] == feat && d[4] == fsw {
                            return Err(ReqErr::Error);
                        }
                        // HID++ 2.0 error: [11, idx, FF, feat, func|sw, code, ...]
                        if d[2] == 0xFF && d[3] == feat && d[4] == fsw {
                            return Err(ReqErr::Error);
                        }
                        if d[2] == feat && d[3] == fsw {
                            return Ok(d);
                        }
                    }
                    self.backlog.push(d);
                }
                Ok(Msg::Gone(id)) if id == self.sess.id => return Err(ReqErr::Gone),
                Ok(Msg::Refresh) => self.refresh = true,
                Ok(_) => {} // leftovers from an older session
                Err(RecvTimeoutError::Timeout) => return Err(ReqErr::Timeout),
                Err(RecvTimeoutError::Disconnected) => return Err(ReqErr::Gone),
            }
        }
    }

    fn ping(&mut self, idx: u8) -> Result<bool, ReqErr> {
        self.ping = self.ping.wrapping_add(1).max(1);
        let payload = self.ping;
        match self.request(idx, 0x00, 1, [0, 0, payload], Duration::from_millis(400)) {
            Ok(d) => Ok(d[6] == payload),
            Err(ReqErr::Gone) => Err(ReqErr::Gone),
            Err(_) => Ok(false),
        }
    }

    fn feature_index(&mut self, idx: u8, feature: u16) -> Result<u8, ReqErr> {
        let d = self.request(idx, 0x00, 0, [(feature >> 8) as u8, feature as u8, 0], Duration::from_millis(500))?;
        Ok(d[4])
    }

    fn device_name(&mut self, idx: u8) -> String {
        let Ok(f) = self.feature_index(idx, 0x0005) else { return "Logitech mouse".into() };
        if f == 0 {
            return "Logitech mouse".into();
        }
        let Ok(d) = self.request(idx, f, 0, [0; 3], Duration::from_millis(500)) else { return "Logitech mouse".into() };
        let count = (d[4] as usize).min(64);
        let mut bytes = Vec::new();
        while bytes.len() < count {
            let Ok(d) = self.request(idx, f, 1, [bytes.len() as u8, 0, 0], Duration::from_millis(500)) else { break };
            let chunk = &d[4..];
            if chunk.is_empty() {
                break;
            }
            bytes.extend_from_slice(chunk);
        }
        bytes.truncate(count);
        let name = String::from_utf8_lossy(&bytes).trim_end_matches('\0').trim().to_string();
        if name.is_empty() { "Logitech mouse".into() } else { name }
    }

    fn probe(&mut self, idx: u8) -> Result<Option<Device>, ReqErr> {
        if !self.ping(idx)? {
            return Ok(None);
        }
        for (id, kind) in [(0x1000u16, BatteryKind::Level), (0x1001, BatteryKind::Voltage), (0x1004, BatteryKind::Unified)] {
            match self.feature_index(idx, id) {
                Ok(f) if f != 0 => {
                    let name = self.device_name(idx);
                    return Ok(Some(Device { idx, feat: f, kind, name }));
                }
                Err(ReqErr::Gone) => return Err(ReqErr::Gone),
                _ => {}
            }
        }
        Ok(None)
    }

    fn read_battery(&mut self, dev: &Device) -> Result<(i32, Charge), ReqErr> {
        let func = if dev.kind == BatteryKind::Unified { 1 } else { 0 };
        let d = self.request(dev.idx, dev.feat, func, [0; 3], Duration::from_millis(1500))?;
        Ok(parse_battery(dev.kind, &d[4..]))
    }
}

fn parse_battery(kind: BatteryKind, p: &[u8]) -> (i32, Charge) {
    match kind {
        BatteryKind::Level | BatteryKind::Unified => {
            let charge = match p[2] {
                0 => Charge::Discharging,
                1 | 2 => Charge::Charging,
                3 => Charge::Full,
                4 if kind == BatteryKind::Level => Charge::Charging,
                _ => Charge::Unknown,
            };
            (p[0] as i32, charge)
        }
        BatteryKind::Voltage => {
            let mv = ((p[0] as i32) << 8) | p[1] as i32;
            let flags = p[2];
            let charge = if flags & 0x80 != 0 {
                match flags & 0x07 {
                    0 => Charge::Charging,
                    1 => Charge::Full,
                    _ => Charge::Unknown,
                }
            } else {
                Charge::Discharging
            };
            (voltage_to_percent(mv), charge)
        }
    }
}

fn voltage_to_percent(mv: i32) -> i32 {
    const LUT: [i32; 100] = [
        4186, 4156, 4143, 4133, 4122, 4113, 4103, 4094, 4086, 4075, 4067, 4059, 4051, 4043, 4035, 4027, 4019, 4011, 4003, 3997,
        3989, 3983, 3976, 3969, 3961, 3955, 3949, 3942, 3935, 3929, 3922, 3916, 3909, 3902, 3896, 3890, 3883, 3877, 3870, 3865,
        3859, 3853, 3848, 3842, 3837, 3833, 3828, 3824, 3819, 3815, 3811, 3808, 3804, 3800, 3797, 3793, 3790, 3787, 3784, 3781,
        3778, 3775, 3772, 3770, 3767, 3764, 3762, 3759, 3757, 3754, 3751, 3748, 3744, 3741, 3737, 3734, 3730, 3726, 3724, 3720,
        3717, 3714, 3710, 3706, 3702, 3697, 3693, 3688, 3683, 3677, 3671, 3666, 3662, 3658, 3654, 3646, 3633, 3612, 3579, 3537,
    ];
    LUT.iter().position(|&v| mv > v).map(|i| 100 - i as i32).unwrap_or(0)
}

pub fn local_hm() -> (u16, u16) {
    unsafe {
        let mut t = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t);
        (t.wHour, t.wMinute)
    }
}

fn set_state(shared: &Shared, f: impl FnOnce(&mut BatteryState)) {
    let changed = {
        let mut s = shared.state.lock().unwrap();
        let before = format!("{:?}", *s);
        f(&mut s);
        before != format!("{:?}", *s)
    };
    if changed {
        (shared.notify)();
    }
}

/// Starts the background worker. Send `Msg::Refresh` to force a battery read.
pub fn spawn(shared: Arc<Shared>) -> Sender<Msg> {
    let (tx, rx) = channel();
    let tx_worker = tx.clone();
    thread::spawn(move || worker(shared, rx, tx_worker));
    tx
}

fn worker(shared: Arc<Shared>, rx: Receiver<Msg>, tx: Sender<Msg>) {
    let mut session_id = 0u32;
    let mut idle_wait = RETRY_PERIOD;
    loop {
        for paths in hid::enumerate() {
            session_id += 1;
            let Some(sess) = Session::open(&paths, session_id, &tx) else { continue };
            let mut ctx = Ctx { rx: &rx, sess: &sess, backlog: Vec::new(), refresh: false, ping: 0x40 };
            'iface: loop {
                let mut found = None;
                for idx in DEVICE_INDEXES {
                    match ctx.probe(idx) {
                        Ok(Some(dev)) => {
                            found = Some(dev);
                            break;
                        }
                        Ok(None) => {}
                        Err(_) => break 'iface, // receiver unplugged
                    }
                }
                let Some(dev) = found else { break 'iface };
                idle_wait = RETRY_PERIOD;
                if !run_device(&shared, &mut ctx, &dev) {
                    break 'iface;
                }
                // Mouse stopped answering for a long time: probe again on the same receiver
            }
            drop(sess);
        }

        set_state(&shared, |s| {
            s.found = false;
            s.online = false;
        });
        // Nothing found: wait (or until "Refresh"), backing off up to 30 s
        let deadline = Instant::now() + idle_wait;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(Msg::Refresh) => break,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        idle_wait = (idle_wait * 2).min(Duration::from_secs(30));
    }
}

/// Polls the device and listens for its battery events. Returns false when the receiver is gone,
/// true when the mouse has been unreachable for a long time (caller re-probes).
fn run_device(shared: &Shared, ctx: &mut Ctx, dev: &Device) -> bool {
    set_state(shared, |s| {
        s.found = true;
        s.name = dev.name.clone();
    });
    let mut next_poll = Instant::now();
    let mut fails = 0u32;

    let apply = |shared: &Shared, (percent, charge): (i32, Charge)| {
        let hm = local_hm();
        set_state(shared, |s| {
            s.online = true;
            s.percent = percent;
            s.charge = charge;
            s.updated = Some(hm);
        });
    };

    loop {
        // Unsolicited messages: battery change events and receiver link notifications
        for d in std::mem::take(&mut ctx.backlog) {
            if d.len() < 7 || d[1] != dev.idx {
                continue;
            }
            if d[2] == dev.feat && d[3] & 0x0F == 0 {
                apply(shared, parse_battery(dev.kind, &d[4..]));
                fails = 0;
            } else if d[2] == 0x41 {
                if d[4] & 0x40 != 0 {
                    set_state(shared, |s| s.online = false); // mouse switched off / out of range
                } else {
                    next_poll = Instant::now() + Duration::from_millis(1500); // mouse came back
                }
            }
        }
        if ctx.refresh {
            ctx.refresh = false;
            next_poll = Instant::now();
        }

        let now = Instant::now();
        if now >= next_poll {
            match ctx.read_battery(dev) {
                Ok(b) => {
                    apply(shared, b);
                    fails = 0;
                    next_poll = Instant::now() + POLL_PERIOD;
                }
                Err(ReqErr::Gone) => return false,
                Err(_) => {
                    fails += 1;
                    if fails == 3 {
                        set_state(shared, |s| s.online = false);
                    }
                    if fails >= 30 {
                        return true; // ~30 min without an answer
                    }
                    next_poll = Instant::now() + if fails < 3 { RETRY_PERIOD } else { POLL_PERIOD };
                }
            }
            continue;
        }

        match ctx.rx.recv_timeout(next_poll - now) {
            Ok(Msg::Report(id, d)) if id == ctx.sess.id => ctx.backlog.push(d),
            Ok(Msg::Gone(id)) if id == ctx.sess.id => return false,
            Ok(Msg::Refresh) => next_poll = Instant::now(),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// One-shot diagnostic used by `MouseBattery.exe --probe`.
pub fn probe_report() -> String {
    let mut out = String::new();
    let (tx, rx) = channel();
    let all = hid::enumerate();
    out += &format!("HID++ interfaces found: {}\n", all.len());
    for (n, paths) in all.iter().enumerate() {
        out += &format!("[{}] short: {}\n    long:  {}\n", n, paths.short, paths.long);
        let Some(sess) = Session::open(paths, 1000 + n as u32, &tx) else {
            out += "    could not open\n";
            continue;
        };
        let mut ctx = Ctx { rx: &rx, sess: &sess, backlog: Vec::new(), refresh: false, ping: 0x20 };
        for idx in DEVICE_INDEXES {
            match ctx.probe(idx) {
                Ok(Some(dev)) => {
                    out += &format!("    index {}: \"{}\" battery feature {:?} at 0x{:02X}\n", idx, dev.name, dev.kind, dev.feat);
                    match ctx.read_battery(&dev) {
                        Ok((p, c)) => out += &format!("    battery: {}% {:?}\n", p, c),
                        Err(_) => out += "    battery read failed\n",
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    out += "    interface gone\n";
                    break;
                }
            }
        }
    }
    out
}
