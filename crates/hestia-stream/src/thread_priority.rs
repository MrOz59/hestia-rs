//! Run the stream's hot threads ahead of ordinary work.
//!
//! The receive threads, the decoder and the audio callback wait on the
//! network or a queue and then have a frame's worth of work to do before the
//! next one is due. At the default priority, a thread woken while the CPU is
//! busy (a browser, a build, the compositor) can wait a scheduler slice or
//! more, and that wait lands on the picture's or the sound's way.
//!
//! A thread is raised by lowering its nice value, never by a real-time
//! scheduling class: a nice value can't freeze the system if a thread spins.
//! An ordinary process may lower its threads' nice values down to what its
//! `RLIMIT_NICE` allows: an Android app to -20, a desktop session anywhere
//! from 0 to that, by distribution. On desktop Linux, when that limit is in
//! the way, RealtimeKit (`rtkit-daemon`, which PipeWire and PulseAudio
//! already use) raises a thread of the asking user's own process to its
//! configured floor (-15 by default).
//!
//! What happened is logged once per thread, so it lands in the session
//! report.

use log::info;

/// What a thread does, which sets how far ahead of ordinary work it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Audio receive and the playback callback: a late wake is a gap.
    Audio,
    /// Video receive and decode.
    Video,
    /// The control stream (input travels on it), controllers, and the
    /// window that draws the video.
    Interactive,
}

impl Role {
    /// The nice value wanted (lower runs earlier; 0 is the default).
    pub const fn nice(self) -> i32 {
        match self {
            Self::Audio => -12,
            Self::Video => -10,
            Self::Interactive => -8,
        }
    }
}

/// How a thread's priority ended up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raised {
    /// Already at this nice value or better: left as it was.
    Already(i32),
    /// The thread's nice value was lowered to this directly.
    Nice(i32),
    /// RealtimeKit lowered the nice value to this.
    Rtkit(i32),
    /// Not raised: no permission, and no RealtimeKit to ask (or it refused).
    Unchanged(i32),
    /// Not on this platform.
    Unsupported,
}

/// The calling thread's kernel id, to raise it from another thread.
pub fn current_thread_id() -> Option<u32> {
    sys::current_thread_id()
}

/// Raise the thread `tid` of this process for `role`, logging the outcome
/// under `name`. Can block for a few milliseconds (RealtimeKit is a D-Bus
/// round trip), so a thread that must never block should hand its id to
/// another one ([`raise_in_background`]).
pub fn raise(name: &str, tid: u32, role: Role) -> Raised {
    let raised = sys::raise(tid, role.nice());
    match raised {
        Raised::Already(nice) => info!("{name}: priority nice {nice} already"),
        Raised::Nice(nice) => info!("{name}: priority raised to nice {nice}"),
        Raised::Rtkit(nice) => info!("{name}: priority raised to nice {nice} by RealtimeKit"),
        Raised::Unchanged(nice) => info!(
            "{name}: priority left at nice {nice} (wanted {}): not permitted (RLIMIT_NICE) and RealtimeKit unavailable",
            role.nice()
        ),
        Raised::Unsupported => {}
    }
    raised
}

/// Raise the calling thread for `role`; see [`raise`].
pub fn raise_current(name: &str, role: Role) -> Raised {
    match current_thread_id() {
        Some(tid) => raise(name, tid, role),
        None => Raised::Unsupported,
    }
}

/// Raise the calling thread for `role` from a short-lived helper thread, for
/// threads that must not block, such as an audio callback.
pub fn raise_in_background(name: &'static str, role: Role) {
    let Some(tid) = current_thread_id() else { return };
    let _ = std::thread::Builder::new()
        .name("hestia-priority".into())
        .spawn(move || raise(name, tid, role));
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod sys {
    use super::Raised;

    const PRIO_PROCESS: i32 = 0;
    const EPERM: i32 = 1;
    const EACCES: i32 = 13;

    unsafe extern "C" {
        fn gettid() -> i32;
        fn setpriority(which: i32, who: u32, prio: i32) -> i32;
    }

    pub(super) fn current_thread_id() -> Option<u32> {
        // SAFETY: gettid has no preconditions.
        u32::try_from(unsafe { gettid() }).ok()
    }

    /// A thread's nice value, from `/proc` (getpriority's -1 is ambiguous
    /// without errno, which std can't reset).
    pub(super) fn nice_of(tid: u32) -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/self/task/{tid}/stat")).ok()?;
        // The command name may hold spaces and parentheses: the fields
        // that follow start after its last `)`, with the state (field 3);
        // the nice value is field 19.
        let after = &stat[stat.rfind(')')? + 1..];
        after.split_whitespace().nth(19 - 3)?.parse().ok()
    }

    pub(super) fn raise(tid: u32, wanted: i32) -> Raised {
        let current = nice_of(tid).unwrap_or(0);
        if current <= wanted {
            return Raised::Already(current);
        }
        // The best value the limit allows, from the wanted one up.
        for nice in wanted..current {
            // SAFETY: setpriority has no memory preconditions.
            if unsafe { setpriority(PRIO_PROCESS, tid, nice) } == 0 {
                return Raised::Nice(nice);
            }
            match std::io::Error::last_os_error().raw_os_error() {
                Some(EACCES | EPERM) => continue,
                _ => break,
            }
        }
        #[cfg(target_os = "linux")]
        if let Some(nice) = rtkit::raise(tid, wanted).and_then(|()| nice_of(tid))
            && nice < current
        {
            return Raised::Rtkit(nice);
        }
        Raised::Unchanged(current)
    }

    /// RealtimeKit over the system bus, asked through `busctl`: a thread of
    /// the caller's own user, by process and thread id.
    #[cfg(target_os = "linux")]
    mod rtkit {
        use std::process::{Command, Stdio};

        /// RealtimeKit's default floor; a request below it is refused.
        const FLOOR: i32 = -15;

        pub(super) fn raise(tid: u32, nice: i32) -> Option<()> {
            let status = Command::new("busctl")
                .args([
                    "--system",
                    "--timeout=2",
                    "call",
                    // The nice value is negative: not an option.
                    "--",
                    "org.freedesktop.RealtimeKit1",
                    "/org/freedesktop/RealtimeKit1",
                    "org.freedesktop.RealtimeKit1",
                    "MakeThreadHighPriorityWithPID",
                    "tti",
                ])
                .arg(std::process::id().to_string())
                .arg(tid.to_string())
                .arg(nice.max(FLOOR).to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .ok()?;
            status.success().then_some(())
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod sys {
    use super::Raised;

    pub(super) fn current_thread_id() -> Option<u32> {
        None
    }

    pub(super) fn raise(_tid: u32, _wanted: i32) -> Raised {
        Raised::Unsupported
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn a_raised_thread_runs_at_the_nice_value_reported() {
        let outcome = std::thread::spawn(|| {
            let tid = current_thread_id().unwrap();
            let before = sys::nice_of(tid).unwrap();
            let raised = raise("test", tid, Role::Video);
            (before, raised, sys::nice_of(tid).unwrap())
        })
        .join()
        .unwrap();
        match outcome {
            (_, Raised::Nice(nice) | Raised::Rtkit(nice), after) => {
                assert_eq!(after, nice);
                assert!(nice <= -1 && nice >= Role::Video.nice());
            }
            // A runner that permits no raise leaves the thread as it was.
            (before, Raised::Unchanged(nice), after) => assert_eq!((before, after), (nice, nice)),
            (before, Raised::Already(nice), after) => assert_eq!((before, after), (nice, nice)),
            (_, Raised::Unsupported, _) => panic!("supported here"),
        }
    }

    #[test]
    fn a_thread_already_ahead_is_left_alone() {
        let raised = std::thread::spawn(|| {
            let tid = current_thread_id().unwrap();
            // Raise for the most urgent role first, then ask for less.
            let first = raise("test", tid, Role::Audio);
            (first, raise("test", tid, Role::Interactive))
        })
        .join()
        .unwrap();
        if let (Raised::Nice(nice) | Raised::Rtkit(nice), second) = raised
            && nice <= Role::Interactive.nice()
        {
            assert_eq!(second, Raised::Already(nice));
        }
    }

    #[test]
    fn nice_values_come_from_the_threads_own_stat_line() {
        let tid = current_thread_id().unwrap();
        assert!(sys::nice_of(tid).is_some_and(|nice| (-20..=19).contains(&nice)));
        assert_eq!(sys::nice_of(u32::MAX), None);
    }
}
