use std::fmt::Write;

use slint::{SharedString, StyledText};

pub(crate) fn spectrum_heading(title: SharedString) -> StyledText {
    const COLORS: [[usize; 3]; 6] = [
        [255, 116, 157],
        [255, 209, 113],
        [122, 251, 184],
        [114, 230, 255],
        [191, 156, 255],
        [255, 133, 239],
    ];
    let steps = title.chars().count().saturating_sub(1).max(1);
    let mut markup = String::from("**");
    for (i, letter) in title.chars().enumerate() {
        let position = i * (COLORS.len() - 1);
        let low = position / steps;
        let high = (low + 1).min(COLORS.len() - 1);
        let mix = position % steps;
        let [r, g, b] = [0, 1, 2].map(|k| (COLORS[low][k] * (steps - mix) + COLORS[high][k] * mix) / steps);
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
