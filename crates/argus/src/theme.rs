//! The TUI's palette: Catppuccin (https://catppuccin.com/palette), Mocha on a
//! dark terminal and Latte on a light one. The terminal's own background is
//! left alone, so agent previews still show through in their real colors;
//! the palette only paints argus's own chrome, text and selection.

use std::sync::OnceLock;

use ratatui::style::Color;

use crate::term;

/// The Catppuccin roles the TUI uses, named as in the upstream palette.
pub struct Theme {
    pub mantle: Color,
    pub crust: Color,
    pub surface0: Color,
    pub surface1: Color,
    pub surface2: Color,
    pub overlay0: Color,
    pub subtext0: Color,
    pub text: Color,
    pub red: Color,
    pub green: Color,
    pub yellow: Color,
    pub mauve: Color,
    pub lavender: Color,
}

const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub const MOCHA: Theme = Theme {
    mantle: hex(0x181825),
    crust: hex(0x11111b),
    surface0: hex(0x313244),
    surface1: hex(0x45475a),
    surface2: hex(0x585b70),
    overlay0: hex(0x6c7086),
    subtext0: hex(0xa6adc8),
    text: hex(0xcdd6f4),
    red: hex(0xf38ba8),
    green: hex(0xa6e3a1),
    yellow: hex(0xf9e2af),
    mauve: hex(0xcba6f7),
    lavender: hex(0xb4befe),
};

pub const LATTE: Theme = Theme {
    mantle: hex(0xe6e9ef),
    crust: hex(0xdce0e8),
    surface0: hex(0xccd0da),
    surface1: hex(0xbcc0cc),
    surface2: hex(0xacb0be),
    overlay0: hex(0x9ca0b0),
    subtext0: hex(0x6c6f85),
    text: hex(0x4c4f69),
    red: hex(0xd20f39),
    green: hex(0x40a02b),
    yellow: hex(0xdf8e1d),
    mauve: hex(0x8839ef),
    lavender: hex(0x7287fd),
};

/// Picked once from the terminal's background (see `term::profile`); Mocha
/// when the terminal did not say.
pub fn theme() -> &'static Theme {
    static THEME: OnceLock<&'static Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let light = term::profile().colors.background.as_deref().and_then(luminance).is_some_and(|l| l > 0.5);
        if light { &LATTE } else { &MOCHA }
    })
}

/// Relative luminance (0–1) of an X11 colour spec as OSC 11 reports it:
/// `rgb:R/G/B` with 1–4 hex digits per channel, or `#RGB`-style hex.
fn luminance(spec: &str) -> Option<f64> {
    let channels: Vec<f64> = if let Some(rest) = spec.strip_prefix("rgb:") {
        rest.split('/').map(channel).collect::<Option<_>>()?
    } else {
        let digits = spec.strip_prefix('#')?;
        if digits.is_empty() || digits.len() % 3 != 0 {
            return None;
        }
        let n = digits.len() / 3;
        (0..3).map(|i| channel(&digits[i * n..(i + 1) * n])).collect::<Option<_>>()?
    };
    let [r, g, b] = channels[..] else { return None };
    Some(0.2126 * r + 0.7152 * g + 0.0722 * b)
}

fn channel(digits: &str) -> Option<f64> {
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let max = (1u32 << (4 * digits.len())) - 1;
    Some(u32::from_str_radix(digits, 16).ok()? as f64 / max as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luminance_reads_osc_11_specs() {
        assert!(luminance("rgb:1e1e/1e1e/2e2e").unwrap() < 0.2);
        assert!(luminance("rgb:ef/f1/f5").unwrap() > 0.9);
        assert!(luminance("#eff1f5").unwrap() > 0.9);
        assert!(luminance("#000").unwrap() < 0.01);
        assert_eq!(luminance("rgb:12/34"), None);
        assert_eq!(luminance("red"), None);
    }
}
