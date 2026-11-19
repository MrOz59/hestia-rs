//! Touch as a laptop trackpad (the usual way to drive a desktop from a
//! phone): the finger moves the host cursor relatively, wherever it is on
//! the screen.
//!
//! * tap → left click
//! * hold still for [`LONG_PRESS`] → right click
//! * tap, then touch again quickly and move → drag with the left button held
//!
//! Pure state machine: the controller feeds it pointer events and a
//! long-press timeout and sends whatever it returns.

use std::time::{Duration, Instant};

use hestia_core::stream::input::MouseButton;

/// Movement under this many logical pixels still counts as a tap.
pub const SLOP: f32 = 10.0;
/// A tap must end within this time.
pub const TAP_TIME: Duration = Duration::from_millis(250);
/// Touching again within this time after a tap starts a drag.
pub const DOUBLE_TAP: Duration = Duration::from_millis(300);
/// Holding still this long is a right click.
pub const LONG_PRESS: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    Move { dx: i16, dy: i16 },
    Button { button: MouseButton, pressed: bool },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Idle,
    /// Finger down; not yet a move, tap or long press.
    Touching,
    /// Moving the cursor.
    Moving,
    /// Left button held (tap-then-drag).
    Dragging,
    /// Long press fired; ignore the rest of this touch.
    Consumed,
}

#[derive(Debug)]
pub struct Trackpad {
    phase: Phase,
    start: (f32, f32),
    last: (f32, f32),
    down_at: Instant,
    /// When the last tap ended (for tap-then-drag).
    last_tap: Option<Instant>,
    /// Second touch of a possible tap-then-drag.
    after_tap: bool,
    /// Sub-pixel movement carried to the next event.
    remainder: (f32, f32),
}

impl Default for Trackpad {
    fn default() -> Self {
        Self::new()
    }
}

fn click(button: MouseButton) -> [Output; 2] {
    [
        Output::Button { button, pressed: true },
        Output::Button { button, pressed: false },
    ]
}

impl Trackpad {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            phase: Phase::Idle,
            start: (0.0, 0.0),
            last: (0.0, 0.0),
            down_at: now,
            last_tap: None,
            after_tap: false,
            remainder: (0.0, 0.0),
        }
    }

    pub fn down(&mut self, x: f32, y: f32, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if self.phase == Phase::Dragging {
            out.push(Output::Button {
                button: MouseButton::Left,
                pressed: false,
            });
        }
        self.after_tap = self.last_tap.is_some_and(|t| now.duration_since(t) <= DOUBLE_TAP);
        self.phase = Phase::Touching;
        self.start = (x, y);
        self.last = (x, y);
        self.down_at = now;
        self.remainder = (0.0, 0.0);
        out
    }

    /// `scale` converts on-screen pixels into host pixels (and speed).
    pub fn moved(&mut self, x: f32, y: f32, scale: f32) -> Vec<Output> {
        let mut out = Vec::new();
        match self.phase {
            Phase::Idle | Phase::Consumed => return out,
            Phase::Touching => {
                let (dx, dy) = (x - self.start.0, y - self.start.1);
                if dx.hypot(dy) < SLOP {
                    return out;
                }
                if self.after_tap {
                    self.phase = Phase::Dragging;
                    out.push(Output::Button {
                        button: MouseButton::Left,
                        pressed: true,
                    });
                } else {
                    self.phase = Phase::Moving;
                }
                // The slop distance itself moves the cursor too.
                self.last = self.start;
            }
            Phase::Moving | Phase::Dragging => {}
        }
        let fx = (x - self.last.0) * scale + self.remainder.0;
        let fy = (y - self.last.1) * scale + self.remainder.1;
        self.last = (x, y);
        let (dx, dy) = (fx.trunc(), fy.trunc());
        self.remainder = (fx - dx, fy - dy);
        if dx != 0.0 || dy != 0.0 {
            out.push(Output::Move {
                dx: dx.clamp(i16::MIN as f32, i16::MAX as f32) as i16,
                dy: dy.clamp(i16::MIN as f32, i16::MAX as f32) as i16,
            });
        }
        out
    }

    pub fn up(&mut self, now: Instant) -> Vec<Output> {
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        match phase {
            Phase::Touching if now.duration_since(self.down_at) <= TAP_TIME => {
                self.last_tap = Some(now);
                click(MouseButton::Left).to_vec()
            }
            Phase::Dragging => {
                self.last_tap = None;
                vec![Output::Button {
                    button: MouseButton::Left,
                    pressed: false,
                }]
            }
            _ => {
                self.last_tap = None;
                Vec::new()
            }
        }
    }

    /// Called [`LONG_PRESS`] after `down`; right-clicks if the finger is
    /// still resting where it landed.
    pub fn long_press(&mut self, now: Instant) -> Vec<Output> {
        if self.phase == Phase::Touching && now.duration_since(self.down_at) + Duration::from_millis(20) >= LONG_PRESS {
            self.phase = Phase::Consumed;
            self.last_tap = None;
            return click(MouseButton::Right).to_vec();
        }
        Vec::new()
    }

    /// The touch was cancelled (e.g. the view went away): release anything held.
    pub fn cancel(&mut self) -> Vec<Output> {
        let held = self.phase == Phase::Dragging;
        *self = Self::new();
        if held {
            vec![Output::Button {
                button: MouseButton::Left,
                pressed: false,
            }]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L_DOWN: Output = Output::Button {
        button: MouseButton::Left,
        pressed: true,
    };
    const L_UP: Output = Output::Button {
        button: MouseButton::Left,
        pressed: false,
    };

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    #[test]
    fn tap_is_a_left_click() {
        let t0 = Instant::now();
        let mut p = Trackpad::new();
        assert!(p.down(100.0, 100.0, t0).is_empty());
        assert!(p.moved(103.0, 101.0, 1.0).is_empty(), "within slop");
        assert_eq!(p.up(ms(t0, 120)), vec![L_DOWN, L_UP]);
    }

    #[test]
    fn slow_still_touch_is_not_a_click() {
        let t0 = Instant::now();
        let mut p = Trackpad::new();
        p.down(0.0, 0.0, t0);
        assert!(p.up(ms(t0, 400)).is_empty());
    }

    #[test]
    fn moves_relatively_with_scale_and_keeps_fractions() {
        let t0 = Instant::now();
        let mut p = Trackpad::new();
        p.down(50.0, 50.0, t0);
        // Crossing the slop moves by the whole distance from the start.
        assert_eq!(p.moved(62.0, 50.0, 2.0), vec![Output::Move { dx: 24, dy: 0 }]);
        assert_eq!(p.moved(62.25, 49.0, 2.0), vec![Output::Move { dx: 0, dy: -2 }]);
        // 0.5 carried + 0.5 → 1
        assert_eq!(p.moved(62.5, 49.0, 2.0), vec![Output::Move { dx: 1, dy: 0 }]);
        assert!(p.up(ms(t0, 900)).is_empty(), "a move is not a click");
    }

    #[test]
    fn long_press_is_a_right_click() {
        let t0 = Instant::now();
        let mut p = Trackpad::new();
        p.down(10.0, 10.0, t0);
        let r = MouseButton::Right;
        assert_eq!(
            p.long_press(ms(t0, 500)),
            vec![
                Output::Button {
                    button: r,
                    pressed: true
                },
                Output::Button {
                    button: r,
                    pressed: false
                }
            ]
        );
        assert!(p.moved(80.0, 80.0, 1.0).is_empty(), "rest of the touch ignored");
        assert!(p.up(ms(t0, 900)).is_empty());
    }

    #[test]
    fn long_press_needs_a_still_finger() {
        let t0 = Instant::now();
        let mut p = Trackpad::new();
        p.down(10.0, 10.0, t0);
        p.moved(40.0, 10.0, 1.0);
        assert!(p.long_press(ms(t0, 500)).is_empty());
    }

    #[test]
    fn tap_then_drag_holds_the_left_button() {
        let t0 = Instant::now();
        let mut p = Trackpad::new();
        p.down(0.0, 0.0, t0);
        assert_eq!(p.up(ms(t0, 80)), vec![L_DOWN, L_UP]);
        p.down(0.0, 0.0, ms(t0, 200));
        assert_eq!(p.moved(20.0, 0.0, 1.0), vec![L_DOWN, Output::Move { dx: 20, dy: 0 }]);
        assert_eq!(p.moved(30.0, 5.0, 1.0), vec![Output::Move { dx: 10, dy: 5 }]);
        assert_eq!(p.up(ms(t0, 900)), vec![L_UP]);
    }

    #[test]
    fn late_second_touch_just_moves() {
        let t0 = Instant::now();
        let mut p = Trackpad::new();
        p.down(0.0, 0.0, t0);
        p.up(ms(t0, 80));
        p.down(0.0, 0.0, ms(t0, 800));
        assert_eq!(p.moved(20.0, 0.0, 1.0), vec![Output::Move { dx: 20, dy: 0 }]);
    }

    #[test]
    fn double_tap_is_two_clicks() {
        let t0 = Instant::now();
        let mut p = Trackpad::new();
        p.down(0.0, 0.0, t0);
        assert_eq!(p.up(ms(t0, 80)), vec![L_DOWN, L_UP]);
        p.down(1.0, 1.0, ms(t0, 150));
        assert_eq!(p.up(ms(t0, 230)), vec![L_DOWN, L_UP]);
    }

    #[test]
    fn cancel_releases_a_drag() {
        let t0 = Instant::now();
        let mut p = Trackpad::new();
        p.down(0.0, 0.0, t0);
        p.up(ms(t0, 50));
        p.down(0.0, 0.0, ms(t0, 100));
        p.moved(30.0, 0.0, 1.0);
        assert_eq!(p.cancel(), vec![L_UP]);
        assert!(p.cancel().is_empty());
    }
}
