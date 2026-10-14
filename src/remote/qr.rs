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

/// The margin around the code in SVG, in modules: the standard's.
const SVG_QUIET: i32 = 4;

/// `text` as a QR code in SVG: dark modules on a light ground with its
/// margin, whatever the page's theme, since that is what readers expect.
/// The image scales to what holds it.
pub fn svg(text: &str) -> Option<String> {
    let code = QrCode::encode_text(text, QrCodeEcc::Low).ok()?;
    let size = code.size();
    let mut path = String::new();
    for y in 0..size {
        for x in 0..size {
            if code.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + SVG_QUIET, y + SVG_QUIET));
            }
        }
    }
    let full = size + 2 * SVG_QUIET;
    Some(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {full} {full}" shape-rendering="crispEdges"><rect width="{full}" height="{full}" fill="#fff"/><path d="{path}" fill="#000"/></svg>"##
    ))
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

    #[test]
    fn the_svg_has_its_margin_and_dark_modules() {
        let svg = svg("http://192.168.1.12:8765/#key=abc").unwrap();
        // 33 bytes: a version 3 code, 29 modules, and the margins.
        assert!(
            svg.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 37 37\""),
            "{svg}"
        );
        // The finder pattern's corner, inside the margin.
        assert!(svg.contains("M4,4h1v1h-1z"));
        assert!(svg.ends_with("</svg>"));
    }
}
