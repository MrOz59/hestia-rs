use std::fmt::Write;

use slint::{SharedString, StyledText};

use crate::VoltagePaths;

pub(crate) fn voltage_paths(width: f32, height: f32, radius: f32, tick: i32) -> VoltagePaths {
    let radius = radius.min(width / 4.0).min(height / 4.0);
    let perimeter = 2.0 * (width + height - 4.0 * radius) + std::f32::consts::TAU * radius;
    if perimeter <= 0.0 {
        return VoltagePaths::default();
    }
    let time = tick as f32 / 30.0;
    let mut arc = String::new();
    let mut forks = String::new();
    for bolt in 0..2 {
        let phase = bolt as f32 * 2.3;
        let head = time * 220.0 + perimeter * bolt as f32 / 2.0;
        let length = (perimeter * 0.27).min(164.0);
        let point = |step: f32, reach: f32| {
            let angle = step * 1.14 - time * 9.0 + phase;
            // The hot strand and its branches cross from inside to outside
            // the panel edge as they wind around it.
            let bend = angle.sin() * 5.0 + (step * 2.7 + time * 18.0 + phase).sin() * 3.0;
            edge_point(
                width,
                height,
                radius,
                (head - length * step / 16.0).rem_euclid(perimeter),
                bend + reach,
            )
        };
        for step in 0..=16 {
            let (x, y) = point(step as f32, 0.0);
            let command = if step == 0 { 'M' } else { 'L' };
            let _ = write!(arc, "{command}{x:.1} {y:.1} ");
        }
        for step in [2.0_f32, 6.0, 11.0] {
            let reach = (step * 2.1 + time * 9.0 + phase).sin() * 12.0;
            let (x1, y1) = point(step, 0.0);
            let (x2, y2) = point(step + 0.8, reach);
            let (x3, y3) = point(step + 2.2, reach * 0.65);
            let _ = write!(forks, "M{x1:.1} {y1:.1} L{x2:.1} {y2:.1} L{x3:.1} {y3:.1} ");
        }
    }
    VoltagePaths {
        arc: arc.into(),
        forks: forks.into(),
    }
}

fn edge_point(width: f32, height: f32, radius: f32, mut distance: f32, offset: f32) -> (f32, f32) {
    let turn = std::f32::consts::FRAC_PI_2 * radius;
    let corners = [
        (width - radius, radius, 1.0, 0.0, width - 2.0 * radius),
        (width - radius, height - radius, 0.0, 1.0, height - 2.0 * radius),
        (radius, height - radius, -1.0, 0.0, width - 2.0 * radius),
        (radius, radius, 0.0, -1.0, height - 2.0 * radius),
    ];
    for (x, y, dx, dy, straight) in corners {
        if distance < straight {
            let along = distance - straight;
            return (
                x + dx * along + dy * (radius + offset),
                y + dy * along - dx * (radius + offset),
            );
        }
        distance -= straight;
        if distance < turn {
            let (sin, cos) = (distance / radius).sin_cos();
            let reach = radius + offset;
            return (x + (dy * cos + dx * sin) * reach, y + (dy * sin - dx * cos) * reach);
        }
        distance -= turn;
    }
    (radius, -offset)
}

pub(crate) fn spectrum_heading(title: SharedString, phase: i32) -> StyledText {
    const COLORS: [[usize; 3]; 7] = [
        [255, 116, 157],
        [255, 209, 113],
        [122, 251, 184],
        [114, 230, 255],
        [191, 156, 255],
        [255, 133, 239],
        [255, 116, 157],
    ];
    const CYCLE: usize = 96;
    let phase = phase.rem_euclid(CYCLE as i32) as usize;
    let steps = title.chars().count().saturating_sub(1).max(1);
    let mut markup = String::from("**");
    for (i, letter) in title.chars().enumerate() {
        let offset = i * 5 * CYCLE / (steps * (COLORS.len() - 1));
        let position = (offset + CYCLE - phase) % CYCLE * (COLORS.len() - 1);
        let stop = position / CYCLE;
        let mix = position % CYCLE;
        let [r, g, b] = [0, 1, 2].map(|k| (COLORS[stop][k] * (CYCLE - mix) + COLORS[stop + 1][k] * mix) / CYCLE);
        // Numeric entities keep title characters separate from the markup.
        let _ = write!(
            markup,
            "<font color='#{r:02x}{g:02x}{b:02x}'>&#{};</font>",
            u32::from(letter)
        );
    }
    markup.push_str("**");
    StyledText::from_markdown(&markup).unwrap_or_else(|_| StyledText::from_plain_text(&title))
}
