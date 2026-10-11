//! A QR code for the terminal, drawn with half blocks.

use qrcodegen::{QrCode, QrCodeEcc};

/// The light margin around the code, in modules.
const QUIET: i32 = 2;

/// `text` as lines of half blocks, two rows of modules a line. The light
/// modules (and the margin) are drawn, the dark ones left blank: on a
/// terminal with a dark background, the code reads dark on light.
pub fn lines(text: &str) -> Option<Vec<String>> {
    let code = QrCode::encode_text(text, QrCodeEcc::Low).ok()?;
    let size = code.size();
    let light = |x: i32, y: i32| x < 0 || y < 0 || x >= size || y >= size || !code.get_module(x, y);
    let mut lines = Vec::new();
    let mut y = -QUIET;
    while y < size + QUIET {
        let line: String = (-QUIET..size + QUIET)
            .map(|x| match (light(x, y), light(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            })
            .collect();
        lines.push(line);
        y += 2;
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_is_square_with_its_margin() {
        let lines =
            lines("http://192.168.1.20:8765/#key=abcdefghijklmnopqrstuvwxyz012345").unwrap();
        let width = lines[0].chars().count();
        // Two rows a line, the last one maybe half.
        assert_eq!(lines.len(), width.div_ceil(2));
        assert!(lines.iter().all(|line| line.chars().count() == width));
        assert!(lines[0].chars().all(|c| c == '█'));
    }
}
