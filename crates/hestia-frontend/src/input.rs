//! Raw input the platform hands to the controller, below the toolkit.
//!
//! Toolkits deliver touches as a mouse: Slint (since 1.16) forwards only the
//! first finger and turns a second one into pinch gestures, and nobody
//! carries a pen's pressure or tilt. So the platform layer (the Android
//! input hook, the desktop shell's winit handler) reads touches and pens
//! itself and passes them on as [`RawPointer`]s, every finger with its own
//! id. The toolkit still sees them too, and still decides what they hit:
//! a touch belongs to the stream only when the toolkit delivered its first
//! finger to the stream view (see [`TouchRouter`]), so the menu button and
//! the keyboard bar keep working.
//!
//! Keys come the same way, by their position on the keyboard (Linux input
//! event codes, `KEY_*`), so the host's layout decides what they type.

/// What touched the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PointerTool {
    #[default]
    Finger,
    Pen,
    /// A pen's eraser end (or a pen held upside down).
    Eraser,
}

impl PointerTool {
    pub fn is_pen(self) -> bool {
        matches!(self, Self::Pen | Self::Eraser)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerPhase {
    Down,
    Move,
    Up,
    Cancel,
    /// A pen above the screen, not touching it.
    Hover,
    /// The pen left the hover range.
    HoverLeave,
}

/// One pointer (finger or pen) at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawPointer {
    /// Unique among the pointers down at the same time.
    pub id: i32,
    pub phase: PointerPhase,
    pub tool: PointerTool,
    /// Position in the window, in physical pixels.
    pub x: f32,
    pub y: f32,
    /// 0.0–1.0, if the device reports it.
    pub pressure: Option<f32>,
    /// Hover distance, 0.0 (touching) – 1.0, if the device reports it.
    pub distance: Option<f32>,
    /// Degrees from perpendicular (0–90), if the device reports it.
    pub tilt: Option<f32>,
    /// Degrees clockwise from the top of the screen, if the device reports it.
    pub rotation: Option<f32>,
    /// Pen buttons held (`hestia_core::stream::input::pen_buttons`).
    pub buttons: u8,
}

impl RawPointer {
    /// A finger at `(x, y)` with nothing else known.
    pub fn finger(id: i32, phase: PointerPhase, x: f32, y: f32) -> Self {
        Self {
            id,
            phase,
            tool: PointerTool::Finger,
            x,
            y,
            pressure: None,
            distance: None,
            tilt: None,
            rotation: None,
            buttons: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Owner {
    /// No finger down.
    #[default]
    Idle,
    /// The first finger is down; the toolkit is deciding what it hit.
    Pending,
    /// The stream view got the first finger: the whole touch is the stream's.
    Stream,
    /// Something else (menu button, keyboard bar, the menu) got it.
    Toolkit,
}

/// Decides which touches belong to the stream.
///
/// A touch (from the first finger down to the last one up) starts with its
/// first finger: [`TouchRouter::raw`] holds it back, the toolkit hit-tests
/// it right after (synchronously, in the same input event), and if the
/// stream view receives it the shell's pointer callback calls
/// [`TouchRouter::claim`], which hands the held finger over. Otherwise the
/// next event finds the touch unclaimed and it stays the toolkit's.
#[derive(Debug, Default)]
pub(crate) struct TouchRouter {
    down: Vec<i32>,
    owner: Owner,
    pending: Option<RawPointer>,
}

impl TouchRouter {
    /// A finger or pen touching the screen (not hovering). Returns what the
    /// stream should handle now.
    pub(crate) fn raw(&mut self, event: RawPointer) -> Option<RawPointer> {
        if self.owner == Owner::Pending {
            // The toolkit had its chance with the first finger.
            self.owner = Owner::Toolkit;
            self.pending = None;
        }
        match event.phase {
            PointerPhase::Down => {
                if self.down.contains(&event.id) {
                    // A lost Up (or a buggy driver): treat it as a move.
                    return (self.owner == Owner::Stream).then_some(RawPointer {
                        phase: PointerPhase::Move,
                        ..event
                    });
                }
                self.down.push(event.id);
                if self.down.len() == 1 {
                    self.owner = Owner::Pending;
                    self.pending = Some(event);
                    return None;
                }
            }
            PointerPhase::Move => {
                if !self.down.contains(&event.id) {
                    return None;
                }
            }
            PointerPhase::Up | PointerPhase::Cancel => {
                if !self.down.contains(&event.id) {
                    return None;
                }
                self.down.retain(|&id| id != event.id);
                let owner = self.owner;
                if self.down.is_empty() {
                    self.owner = Owner::Idle;
                }
                return (owner == Owner::Stream).then_some(event);
            }
            PointerPhase::Hover | PointerPhase::HoverLeave => return None,
        }
        (self.owner == Owner::Stream).then_some(event)
    }

    /// The toolkit delivered a touch to the stream view. Returns the first
    /// finger it was holding back, if this is a new touch.
    pub(crate) fn claim(&mut self) -> Option<RawPointer> {
        if self.owner != Owner::Pending {
            return None;
        }
        self.owner = Owner::Stream;
        self.pending.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(id: i32, phase: PointerPhase) -> RawPointer {
        RawPointer::finger(id, phase, 10.0 * id as f32, 5.0)
    }

    #[test]
    fn claimed_touch_reaches_the_stream_with_every_finger() {
        let mut r = TouchRouter::default();
        assert_eq!(r.raw(at(0, PointerPhase::Down)), None, "held back for the toolkit");
        assert_eq!(r.claim(), Some(at(0, PointerPhase::Down)));
        assert_eq!(r.claim(), None, "only once");
        assert_eq!(r.raw(at(1, PointerPhase::Down)), Some(at(1, PointerPhase::Down)));
        assert_eq!(r.raw(at(0, PointerPhase::Move)), Some(at(0, PointerPhase::Move)));
        assert_eq!(r.raw(at(0, PointerPhase::Up)), Some(at(0, PointerPhase::Up)));
        assert_eq!(r.raw(at(1, PointerPhase::Move)), Some(at(1, PointerPhase::Move)));
        assert_eq!(r.raw(at(1, PointerPhase::Up)), Some(at(1, PointerPhase::Up)));
        // The next touch starts over.
        assert_eq!(r.raw(at(2, PointerPhase::Down)), None);
        assert_eq!(r.claim().map(|p| p.id), Some(2));
    }

    #[test]
    fn unclaimed_touch_stays_with_the_toolkit() {
        let mut r = TouchRouter::default();
        assert_eq!(r.raw(at(0, PointerPhase::Down)), None);
        // The menu button took it: no claim before the next event.
        assert_eq!(r.raw(at(0, PointerPhase::Move)), None);
        assert_eq!(r.claim(), None, "too late");
        assert_eq!(r.raw(at(1, PointerPhase::Down)), None, "second finger too");
        assert_eq!(r.raw(at(1, PointerPhase::Up)), None);
        assert_eq!(r.raw(at(0, PointerPhase::Up)), None);
        assert_eq!(r.raw(at(3, PointerPhase::Down)), None);
        assert!(r.claim().is_some(), "a new touch can be claimed");
    }

    #[test]
    fn ignores_strays_and_repeated_downs() {
        let mut r = TouchRouter::default();
        assert_eq!(r.raw(at(4, PointerPhase::Move)), None);
        assert_eq!(r.raw(at(4, PointerPhase::Up)), None);
        r.raw(at(0, PointerPhase::Down));
        r.claim();
        assert_eq!(
            r.raw(at(0, PointerPhase::Down)).map(|p| p.phase),
            Some(PointerPhase::Move),
            "a second Down for the same finger is a move"
        );
        assert_eq!(
            r.raw(at(0, PointerPhase::Cancel)).map(|p| p.phase),
            Some(PointerPhase::Cancel)
        );
        assert_eq!(r.raw(at(0, PointerPhase::Hover)), None);
    }
}
