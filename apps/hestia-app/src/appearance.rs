use std::fmt::Write;

use slint::{SharedString, StyledText};

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
