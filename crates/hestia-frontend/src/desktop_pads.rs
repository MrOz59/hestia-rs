//! Game controllers on desktop Linux, read straight from evdev
//! (`/dev/input/event*`, the nodes logind opens to the logged-in user, as
//! SDL and Steam read them): no libudev or SDL. Each controller takes a
//! slot like on Android and reaches the host only while streaming; hot
//! plugging is picked up every couple of seconds. Rumble goes through the
//! kernel's force feedback (`FF_RUMBLE`).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use log::{debug, info};

use crate::padmap::{self, Layout, PadState, Range};
use crate::platform;

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const EV_FF: u16 = 0x15;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const FF_RUMBLE: u16 = 0x50;

const RESCAN: Duration = Duration::from_secs(2);
const POLL_MS: i32 = 50;

// <linux/ioctl.h> request numbers for the 'E' (evdev) family.
fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | (u64::from(b'E') << 8) | nr
}
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

/// `struct ff_effect` for FF_RUMBLE: the union (whose largest member holds
/// a pointer) as four u64s, the rumble magnitudes in its first eight bytes.
#[repr(C)]
#[derive(Default)]
#[allow(dead_code)] // read by the kernel
struct FfEffect {
    kind: u16,
    id: i16,
    direction: u16,
    trigger_button: u16,
    trigger_interval: u16,
    replay_length: u16,
    replay_delay: u16,
    u: [u64; 4],
}

struct Pad {
    file: File,
    path: PathBuf,
    name: String,
    /// Slot key in the shared controller table.
    id: i32,
    layout: Layout,
    ranges: [Option<Range>; 0x20],
    state: PadState,
    sync_lost: bool,
    sent: Option<(u32, (i16, i16), (i16, i16), (u8, u8))>,
    rumble: Option<i16>,
    can_rumble: bool,
}

/// Rumble requests from the stream, applied by the controller thread.
static RUMBLE: Mutex<Vec<(i32, u16, u16)>> = Mutex::new(Vec::new());
static STREAM_SERIAL: AtomicUsize = AtomicUsize::new(0);

pub fn stream_started() {
    STREAM_SERIAL.fetch_add(1, Ordering::Release);
}

/// The host asked controller `device` to rumble (-1: every controller).
pub fn rumble(device: i32, low: u16, high: u16) {
    RUMBLE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((device, low, high));
}

fn has_bit(bits: &[u8], n: usize) -> bool {
    bits.get(n / 8).is_some_and(|b| b & (1 << (n % 8)) != 0)
}

fn abs_info(fd: RawFd, axis: u16) -> io::Result<libc::input_absinfo> {
    // SAFETY: EVIOCGABS writes one input_absinfo to the provided buffer.
    let mut info: libc::input_absinfo = unsafe { std::mem::zeroed() };
    let request = ioc(IOC_READ, 0x40 + u64::from(axis), std::mem::size_of_val(&info));
    if unsafe { libc::ioctl(fd, request as _, &mut info) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

impl Pad {
    /// Opens `path` if it is a game controller.
    fn open(path: PathBuf, id: i32) -> Option<Self> {
        let open = |write| {
            OpenOptions::new()
                .read(true)
                .write(write)
                .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
        };
        let (file, can_write) = match open(true) {
            Ok(f) => (f, true),
            Err(_) => (open(false).ok()?, false),
        };
        let fd = file.as_raw_fd();
        let mut keys = [0u8; 0x300 / 8];
        // SAFETY: EVIOCGBIT fills at most `keys.len()` bytes.
        let n = unsafe {
            libc::ioctl(
                fd,
                ioc(IOC_READ, 0x20 + u64::from(EV_KEY), keys.len()) as _,
                keys.as_mut_ptr(),
            )
        };
        if n < 0 || !has_bit(&keys, usize::from(padmap::BTN_SOUTH)) {
            return None;
        }
        let mut name = [0u8; 128];
        // SAFETY: EVIOCGNAME writes a NUL-terminated name within the buffer.
        unsafe { libc::ioctl(fd, ioc(IOC_READ, 0x06, name.len()) as _, name.as_mut_ptr()) };
        let name = String::from_utf8_lossy(name.split(|&b| b == 0).next().unwrap_or_default()).into_owned();

        let mut abs = [0u8; 0x40 / 8];
        // SAFETY: as above, for the absolute axes.
        unsafe {
            libc::ioctl(
                fd,
                ioc(IOC_READ, 0x20 + u64::from(EV_ABS), abs.len()) as _,
                abs.as_mut_ptr(),
            )
        };
        let mut ranges = [None; 0x20];
        for axis in padmap::AXES {
            if !has_bit(&abs, usize::from(axis)) {
                continue;
            }
            if let Ok(info) = abs_info(fd, axis) {
                ranges[usize::from(axis)] = Some((info.minimum, info.maximum));
            }
        }
        let layout = Layout::detect(|axis| ranges.get(usize::from(axis)).is_some_and(Option::is_some));

        let mut ff = [0u8; 0x80 / 8];
        // SAFETY: as above, for force-feedback capabilities.
        unsafe {
            libc::ioctl(
                fd,
                ioc(IOC_READ, 0x20 + u64::from(EV_FF), ff.len()) as _,
                ff.as_mut_ptr(),
            )
        };
        let can_rumble = can_write && has_bit(&ff, usize::from(FF_RUMBLE));
        let mut pad = Self {
            file,
            path,
            name,
            id,
            layout,
            ranges,
            state: PadState::default(),
            sync_lost: false,
            sent: None,
            rumble: None,
            can_rumble,
        };
        if let Err(e) = pad.sync_state() {
            debug!("controller state unavailable on {}: {e}", pad.path.display());
            return None;
        }
        info!(
            "controller connected: {} ({}){}",
            pad.name,
            pad.path.display(),
            if can_rumble { ", rumble" } else { "" }
        );
        Some(pad)
    }

    /// EVIOCGKEY and EVIOCGABS report the current state, including releases
    /// that may have been lost when the evdev queue overflowed.
    fn sync_state(&mut self) -> io::Result<()> {
        let fd = self.file.as_raw_fd();
        let mut keys = [0u8; 0x300 / 8];
        // SAFETY: EVIOCGKEY writes up to keys.len() bytes.
        if unsafe { libc::ioctl(fd, ioc(IOC_READ, 0x18, keys.len()) as _, keys.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut state = PadState::default();
        for code in padmap::BTN_SOUTH..=padmap::BTN_DPAD_RIGHT {
            if has_bit(&keys, usize::from(code)) {
                state.key(code, true);
            }
        }
        for axis in padmap::AXES {
            if let Some(range) = self.ranges[usize::from(axis)] {
                state.axis(&self.layout, axis, abs_info(fd, axis)?.value, range);
            }
        }
        self.state = state;
        Ok(())
    }

    /// Reads what's pending; false once the device is gone.
    fn read(&mut self) -> bool {
        // SAFETY: input_event is plain old data.
        let mut events: [libc::input_event; 64] = unsafe { std::mem::zeroed() };
        'read: loop {
            // SAFETY: reads whole input_event structs into the buffer.
            let n = unsafe {
                libc::read(
                    self.file.as_raw_fd(),
                    events.as_mut_ptr().cast(),
                    std::mem::size_of_val(&events),
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                return e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::Interrupted;
            }
            if n == 0 {
                return false;
            }
            let count = n as usize / std::mem::size_of::<libc::input_event>();
            for e in &events[..count] {
                if self.sync_lost {
                    if e.type_ == EV_SYN && e.code == SYN_REPORT {
                        match self.sync_state() {
                            Ok(()) => {
                                self.sync_lost = false;
                                self.report();
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) => {
                                debug!("controller resync failed on {}: {e}", self.path.display());
                                return false;
                            }
                        }
                        // The snapshot supersedes events already read in this batch.
                        continue 'read;
                    }
                    continue;
                }
                match e.type_ {
                    EV_KEY => self.state.key(e.code, e.value != 0),
                    EV_ABS => {
                        if let Some(Some(range)) = self.ranges.get(usize::from(e.code)) {
                            self.state.axis(&self.layout, e.code, e.value, *range);
                        }
                    }
                    EV_SYN if e.code == SYN_DROPPED => self.sync_lost = true,
                    EV_SYN if e.code == SYN_REPORT => self.report(),
                    _ => {}
                }
            }
            if count < events.len() {
                return true;
            }
        }
    }

    /// A complete update: to the host when it changed (and a stream runs).
    fn report(&mut self) {
        let now = self.state.snapshot();
        if self.sent == Some(now) {
            return;
        }
        self.sent = Some(now);
        let (buttons, left, right, triggers) = now;
        platform::gamepad_state(self.id, buttons, left, right, triggers);
    }

    /// Starts, changes or stops the rumble (strong = low frequency motor).
    fn set_rumble(&mut self, low: u16, high: u16) {
        if !self.can_rumble {
            return;
        }
        let fd = self.file.as_raw_fd();
        if low == 0 && high == 0 {
            if let Some(id) = self.rumble {
                self.play(id, false);
            }
            return;
        }
        let mut effect = FfEffect {
            kind: FF_RUMBLE,
            id: self.rumble.unwrap_or(-1),
            // 0: plays until stopped or changed.
            replay_length: 0,
            ..FfEffect::default()
        };
        effect.u[0] = u64::from(low) | (u64::from(high) << 16);
        // SAFETY: EVIOCSFF reads (and updates the id of) a struct ff_effect.
        let request = ioc(IOC_WRITE, 0x80, std::mem::size_of::<FfEffect>());
        if unsafe { libc::ioctl(fd, request as _, &mut effect) } < 0 {
            debug!(
                "rumble upload failed on {}: {}",
                self.name,
                std::io::Error::last_os_error()
            );
            self.can_rumble = false;
            return;
        }
        self.rumble = Some(effect.id);
        self.play(effect.id, true);
    }

    fn play(&self, id: i16, on: bool) {
        // SAFETY: input_event is plain old data.
        let mut event: libc::input_event = unsafe { std::mem::zeroed() };
        event.type_ = EV_FF;
        event.code = id as u16;
        event.value = i32::from(on);
        // SAFETY: writes one whole input_event.
        unsafe {
            libc::write(
                self.file.as_raw_fd(),
                (&event as *const libc::input_event).cast(),
                std::mem::size_of::<libc::input_event>(),
            )
        };
    }
}

/// Watches for controllers on a thread of its own.
pub fn start() {
    let spawned = std::thread::Builder::new().name("hestia-gamepads".into()).spawn(run);
    if let Err(e) = spawned {
        log::warn!("controller support unavailable: {e}");
    }
}

fn scan(pads: &mut Vec<Pad>, next_id: &mut i32) {
    let Ok(dir) = std::fs::read_dir("/dev/input") else {
        return;
    };
    for entry in dir.flatten() {
        let path = entry.path();
        let is_event = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("event"));
        if !is_event || pads.iter().any(|p| p.path == path) {
            continue;
        }
        if let Some(mut pad) = Pad::open(path, *next_id) {
            *next_id += 1;
            pad.report();
            pads.push(pad);
        }
    }
}

fn run() {
    let mut pads: Vec<Pad> = Vec::new();
    let mut next_id = 1;
    let mut scanned: Option<Instant> = None;
    let mut stream_serial = STREAM_SERIAL.load(Ordering::Acquire);
    loop {
        if scanned.is_none_or(|at| at.elapsed() >= RESCAN) {
            scan(&mut pads, &mut next_id);
            scanned = Some(Instant::now());
        }
        let current_serial = STREAM_SERIAL.load(Ordering::Acquire);
        if current_serial != stream_serial {
            stream_serial = current_serial;
            for pad in &mut pads {
                pad.sent = None;
                pad.report();
            }
        }
        let requests = std::mem::take(&mut *RUMBLE.lock().unwrap_or_else(|p| p.into_inner()));
        for (device, low, high) in requests {
            for pad in pads.iter_mut().filter(|p| device < 0 || p.id == device) {
                pad.set_rumble(low, high);
            }
        }
        if pads.is_empty() {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let mut fds: Vec<libc::pollfd> = pads
            .iter()
            .map(|p| libc::pollfd {
                fd: p.file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: `fds` is a valid array of pollfd for its length.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, POLL_MS) };
        if ready <= 0 {
            continue;
        }
        let mut gone = Vec::new();
        for (i, fd) in fds.iter().enumerate() {
            let broken = fd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0;
            if broken || (fd.revents & libc::POLLIN != 0 && !pads[i].read()) {
                gone.push(i);
            }
        }
        for i in gone.into_iter().rev() {
            let pad = pads.remove(i);
            info!("controller disconnected: {}", pad.name);
            platform::gamepad_gone(pad.id);
        }
    }
}
