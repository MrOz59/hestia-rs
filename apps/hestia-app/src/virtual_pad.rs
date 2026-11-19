//! On-screen controller (as in Moonlight/Artemis): sticks, d-pad, face
//! buttons, bumpers, triggers and Back/Home/Start drawn over the stream.
//!
//! Pure logic: the layout for a view size, which control each finger is
//! on, and the controller state that results. Every finger is followed on
//! its own, so a thumb on each stick (or a stick and a button) works.

use std::time::{Duration, Instant};

use hestia_core::stream::input::buttons;

/// A tap shorter than this on a stick clicks it (L3/R3).
const STICK_TAP: Duration = Duration::from_millis(220);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A round face button (A/B/X/Y).
    Round(u32),
    /// A rectangular button (bumpers, Back/Home/Start).
    Rect(u32),
    Trigger(Side),
    Stick(Side),
    Dpad,
}

/// A control, centred on (`x`, `y`); `w`×`h` for buttons, `w` = diameter
/// for sticks and the d-pad.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Control {
    pub kind: Kind,
    pub label: &'static str,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Control {
    fn is_button(&self) -> bool {
        matches!(self.kind, Kind::Round(_) | Kind::Rect(_) | Kind::Trigger(_))
    }

    /// How far a point is from the centre, in control sizes.
    fn distance(&self, x: f32, y: f32) -> f32 {
        ((x - self.x) / self.w).hypot((y - self.y) / self.h)
    }

    fn hit(&self, x: f32, y: f32) -> bool {
        let (dx, dy) = (x - self.x, y - self.y);
        match self.kind {
            Kind::Round(_) => dx.hypot(dy) <= self.w * 0.6,
            Kind::Rect(_) | Kind::Trigger(_) => dx.abs() <= self.w * 0.55 && dy.abs() <= self.h * 0.6,
            // Generous: thumbs land off-centre.
            Kind::Stick(_) | Kind::Dpad => dx.hypot(dy) <= self.w * 0.65,
        }
    }
}

/// Portrait from this height/width ratio on: the controller sits under the
/// picture instead of around it.
pub const PORTRAIT_RATIO: f32 = 1.3;

/// The layout for a `width`×`height` view (logical px). Landscape: d-pad
/// over the left stick, face buttons over the right one, bumpers and
/// triggers in the top corners, Back/Home/Start at the bottom centre; the
/// top centre stays free for the menu handle. Portrait: the same clusters
/// at the bottom, with a shoulder row and Back/Home/Start above them.
pub fn layout(width: f32, height: f32) -> Vec<Control> {
    let (w, h) = (width, height);
    let portrait = h >= PORTRAIT_RATIO * w;
    // Size unit: the short side, shrunk where the screen is too square for
    // the clusters and the middle row to sit side by side.
    let s = if portrait {
        w.min(h / 2.2)
    } else {
        w.min(h).min(w / 1.65)
    };
    let stick = 0.34 * s;
    let stick_y = h - 0.27 * s;
    let upper_y = h - 0.66 * s;
    let face = 0.15 * s;
    let gap = 0.12 * s;
    let (left_x, right_x) = (0.30 * s, w - 0.30 * s);
    let shoulder_h = 0.10 * s;
    let (small_w, small_h) = (0.19 * s, 0.09 * s);
    let (shoulder_x, shoulder_y, shoulder_w, small_y) = if portrait {
        let y = upper_y - 0.30 * s;
        (
            [0.13 * w, 0.37 * w, 0.63 * w, 0.87 * w],
            y,
            (0.22 * w).min(0.26 * s),
            y - 0.12 * s,
        )
    } else {
        let x = [0.15 * s, 0.42 * s, w - 0.42 * s, w - 0.15 * s];
        (x, 0.07 * s, 0.24 * s, h - 0.08 * s)
    };
    let c = |kind, label, x, y, w, h| Control {
        kind,
        label,
        x,
        y,
        w,
        h,
    };
    let shoulder = |kind, label, i: usize| c(kind, label, shoulder_x[i], shoulder_y, shoulder_w, shoulder_h);
    let small = |kind, label, dx: f32| c(kind, label, w / 2.0 + dx * s, small_y, small_w, small_h);
    vec![
        c(Kind::Stick(Side::Left), "", left_x, stick_y, stick, stick),
        c(Kind::Dpad, "", left_x, upper_y, stick, stick),
        c(Kind::Stick(Side::Right), "", right_x, stick_y, stick, stick),
        c(Kind::Round(buttons::Y), "Y", right_x, upper_y - gap, face, face),
        c(Kind::Round(buttons::A), "A", right_x, upper_y + gap, face, face),
        c(Kind::Round(buttons::X), "X", right_x - gap, upper_y, face, face),
        c(Kind::Round(buttons::B), "B", right_x + gap, upper_y, face, face),
        shoulder(Kind::Trigger(Side::Left), "LT", 0),
        shoulder(Kind::Rect(buttons::LB), "LB", 1),
        shoulder(Kind::Rect(buttons::RB), "RB", 2),
        shoulder(Kind::Trigger(Side::Right), "RT", 3),
        small(Kind::Rect(buttons::BACK), "Back", -0.22),
        small(Kind::Rect(buttons::SPECIAL), "Home", 0.0),
        small(Kind::Rect(buttons::PLAY), "Start", 0.22),
    ]
}

/// What the pad reports: buttons, sticks (GameStream axes: Y up) and
/// triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PadState {
    pub buttons: u32,
    pub left_stick: (i16, i16),
    pub right_stick: (i16, i16),
    pub left_trigger: u8,
    pub right_trigger: u8,
}

#[derive(Debug, Clone, Copy)]
struct Finger {
    id: i32,
    control: usize,
    started: Instant,
    /// Furthest the finger got from where it landed, as a share of the
    /// control's radius (tells a stick tap from a push).
    travel: f32,
    x: f32,
    y: f32,
}

#[derive(Debug, Default)]
pub struct VirtualPad {
    controls: Vec<Control>,
    fingers: Vec<Finger>,
    /// A stick click to send as a press then a release.
    click: u32,
}

impl VirtualPad {
    pub fn resize(&mut self, width: f32, height: f32) {
        self.controls = layout(width, height);
        self.fingers.clear();
    }

    pub fn controls(&self) -> &[Control] {
        &self.controls
    }

    pub fn owns(&self, finger: i32) -> bool {
        self.fingers.iter().any(|f| f.id == finger)
    }

    /// A finger landed; true if on a control (the pad takes it).
    pub fn down(&mut self, finger: i32, x: f32, y: f32, now: Instant) -> bool {
        // Buttons first, then the bigger sticks and d-pad; the nearest one
        // where touch areas meet.
        let nearest = |buttons: bool| {
            self.controls
                .iter()
                .enumerate()
                .filter(|(_, c)| c.is_button() == buttons && c.hit(x, y))
                .min_by(|(_, a), (_, b)| a.distance(x, y).total_cmp(&b.distance(x, y)))
                .map(|(i, _)| i)
        };
        let Some(control) = nearest(true).or_else(|| nearest(false)) else {
            return false;
        };
        self.fingers.retain(|f| f.id != finger);
        self.fingers.push(Finger {
            id: finger,
            control,
            started: now,
            travel: 0.0,
            x,
            y,
        });
        true
    }

    pub fn moved(&mut self, finger: i32, x: f32, y: f32) {
        let controls = &self.controls;
        if let Some(f) = self.fingers.iter_mut().find(|f| f.id == finger) {
            let c = controls[f.control];
            let radius = (c.w / 2.0).max(1.0);
            f.travel = f.travel.max((x - c.x).hypot(y - c.y) / radius);
            (f.x, f.y) = (x, y);
        }
    }

    /// The finger lifted (or was cancelled).
    pub fn up(&mut self, finger: i32, now: Instant) {
        let Some(i) = self.fingers.iter().position(|f| f.id == finger) else {
            return;
        };
        let f = self.fingers.remove(i);
        if let Kind::Stick(side) = self.controls[f.control].kind
            && now.duration_since(f.started) < STICK_TAP
            && f.travel < 0.35
        {
            self.click |= match side {
                Side::Left => buttons::LS_CLICK,
                Side::Right => buttons::RS_CLICK,
            };
        }
    }

    /// A pending stick click, taken once (send it pressed, then released).
    pub fn take_click(&mut self) -> u32 {
        std::mem::take(&mut self.click)
    }

    fn stick_value(&self, c: &Control, f: &Finger) -> (i16, i16) {
        let radius = (c.w / 2.0).max(1.0);
        let (mut dx, mut dy) = ((f.x - c.x) / radius, (f.y - c.y) / radius);
        let length = dx.hypot(dy);
        if length > 1.0 {
            (dx, dy) = (dx / length, dy / length);
        }
        let axis = |v: f32| (v * 32767.0).round().clamp(-32767.0, 32767.0) as i16;
        (axis(dx), axis(-dy))
    }

    fn dpad_value(c: &Control, f: &Finger) -> u32 {
        let (dx, dy) = (f.x - c.x, f.y - c.y);
        let distance = dx.hypot(dy);
        if distance < c.w * 0.1 {
            return 0;
        }
        // 8-way: a direction counts when within ~66° of it.
        let mut bits = 0;
        if dx > 0.4 * distance {
            bits |= buttons::RIGHT;
        } else if dx < -0.4 * distance {
            bits |= buttons::LEFT;
        }
        if dy > 0.4 * distance {
            bits |= buttons::DOWN;
        } else if dy < -0.4 * distance {
            bits |= buttons::UP;
        }
        bits
    }

    pub fn state(&self) -> PadState {
        let mut s = PadState::default();
        for f in &self.fingers {
            let c = &self.controls[f.control];
            match c.kind {
                Kind::Round(bit) | Kind::Rect(bit) => s.buttons |= bit,
                Kind::Trigger(Side::Left) => s.left_trigger = 255,
                Kind::Trigger(Side::Right) => s.right_trigger = 255,
                Kind::Stick(Side::Left) => s.left_stick = self.stick_value(c, f),
                Kind::Stick(Side::Right) => s.right_stick = self.stick_value(c, f),
                Kind::Dpad => s.buttons |= Self::dpad_value(c, f),
            }
        }
        s
    }

    /// For drawing: whether control `i` is held, and for sticks the knob's
    /// offset from the centre in px.
    pub fn view(&self, i: usize) -> (bool, (f32, f32), u32) {
        let Some(c) = self.controls.get(i) else {
            return (false, (0.0, 0.0), 0);
        };
        let Some(f) = self.fingers.iter().find(|f| f.control == i) else {
            return (false, (0.0, 0.0), 0);
        };
        match c.kind {
            Kind::Stick(_) => {
                let (x, y) = self.stick_value(c, f);
                let r = c.w / 2.0;
                (true, (f32::from(x) / 32767.0 * r, -f32::from(y) / 32767.0 * r), 0)
            }
            Kind::Dpad => (true, (0.0, 0.0), Self::dpad_value(c, f)),
            _ => (true, (0.0, 0.0), 0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad() -> (VirtualPad, Vec<Control>) {
        let mut p = VirtualPad::default();
        p.resize(1000.0, 500.0);
        let controls = p.controls().to_vec();
        (p, controls)
    }

    fn find(controls: &[Control], kind: Kind) -> Control {
        *controls.iter().find(|c| c.kind == kind).unwrap()
    }

    #[test]
    fn layout_fits_and_doesnt_overlap() {
        // Phones both ways, 4:3 and near-square tablets, foldables, desktop.
        let sizes = [
            (915.0, 412.0),
            (412.0, 915.0),
            (1000.0, 500.0),
            (2000.0, 900.0),
            (800.0, 1200.0),
            (1024.0, 768.0),
            (768.0, 1024.0),
            (900.0, 1000.0),
            (1180.0, 760.0),
        ];
        for (w, h) in sizes {
            let controls = layout(w, h);
            let rect = |c: &Control| (c.x - c.w / 2.0, c.y - c.h / 2.0, c.x + c.w / 2.0, c.y + c.h / 2.0);
            for c in &controls {
                let (l, t, r, b) = rect(c);
                assert!(l >= 0.0 && r <= w && t >= 0.0 && b <= h, "{c:?} in {w}x{h}");
                // The menu handle's place (top centre) stays free.
                let handle = (w / 2.0 - 34.0, 0.0, w / 2.0 + 34.0, 70.0);
                assert!(
                    r <= handle.0 || l >= handle.2 || t >= handle.3,
                    "{c:?} under the menu handle in {w}x{h}"
                );
            }
            // No two controls are drawn over each other (circles as circles).
            let round = |c: &Control| matches!(c.kind, Kind::Round(_) | Kind::Stick(_));
            for (i, a) in controls.iter().enumerate() {
                for b in &controls[i + 1..] {
                    let ((al, at, ar, ab), (bl, bt, br, bb)) = (rect(a), rect(b));
                    let apart = if round(a) && round(b) {
                        (a.x - b.x).hypot(a.y - b.y) >= (a.w + b.w) / 2.0
                    } else {
                        ar <= bl || br <= al || ab <= bt || bb <= at
                    };
                    assert!(apart, "{:?} and {:?} overlap in {w}x{h}", a.kind, b.kind);
                }
            }
            // Every control's centre hits only itself.
            for (i, c) in controls.iter().enumerate() {
                let hits: Vec<usize> = (0..controls.len()).filter(|&j| controls[j].hit(c.x, c.y)).collect();
                assert_eq!(hits, vec![i], "{:?}", c.kind);
            }
        }
    }

    #[test]
    fn buttons_and_two_thumbs() {
        let (mut p, controls) = pad();
        let t0 = Instant::now();
        let a = find(&controls, Kind::Round(buttons::A));
        let stick = find(&controls, Kind::Stick(Side::Left));
        assert!(p.down(1, a.x, a.y, t0));
        assert!(p.down(2, stick.x, stick.y, t0));
        p.moved(2, stick.x + stick.w, stick.y); // pushed fully right
        let s = p.state();
        assert_eq!(s.buttons, buttons::A);
        assert_eq!(s.left_stick, (32767, 0));
        p.up(1, t0 + Duration::from_millis(500));
        assert_eq!(p.state().buttons, 0);
        assert!(!p.down(3, 500.0, 250.0, t0), "the middle of the screen is free");
    }

    #[test]
    fn stick_up_is_positive_and_tap_clicks() {
        let (mut p, controls) = pad();
        let t0 = Instant::now();
        let stick = find(&controls, Kind::Stick(Side::Right));
        p.down(1, stick.x, stick.y, t0);
        p.moved(1, stick.x, stick.y - stick.w / 4.0);
        let (x, y) = p.state().right_stick;
        assert_eq!(x, 0);
        assert!((16000..=16500).contains(&y), "{y}");
        p.up(1, t0 + Duration::from_millis(600));
        assert_eq!(p.take_click(), 0, "a push isn't a click");

        p.down(1, stick.x, stick.y, t0);
        p.up(1, t0 + Duration::from_millis(100));
        assert_eq!(p.take_click(), buttons::RS_CLICK);
        assert_eq!(p.take_click(), 0, "taken once");
    }

    #[test]
    fn dpad_directions_and_triggers() {
        let (mut p, controls) = pad();
        let t0 = Instant::now();
        let d = find(&controls, Kind::Dpad);
        p.down(1, d.x, d.y - d.w / 3.0, t0);
        p.moved(1, d.x, d.y - d.w / 3.0);
        assert_eq!(p.state().buttons, buttons::UP);
        p.moved(1, d.x + d.w / 3.0, d.y + d.w / 3.0);
        assert_eq!(p.state().buttons, buttons::RIGHT | buttons::DOWN);
        let lt = find(&controls, Kind::Trigger(Side::Left));
        p.down(2, lt.x, lt.y, t0);
        assert_eq!(p.state().left_trigger, 255);
        assert_eq!(
            p.view(controls.iter().position(|c| c.kind == Kind::Dpad).unwrap()).2,
            buttons::RIGHT | buttons::DOWN
        );
    }
}
