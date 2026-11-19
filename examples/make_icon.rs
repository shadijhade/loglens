//! Rebuilds `assets/icon.ico` from the two drawings next to it:
//!
//!     cargo run --example make_icon
//!
//! Sizes up to 32 px come from the simplified drawing, larger ones from the full one.

use resvg::{tiny_skia, usvg};

const SIZES: [(u32, &str); 10] = [
    (16, "icon-small"),
    (20, "icon-small"),
    (24, "icon-small"),
    (32, "icon-small"),
    (40, "icon"),
    (48, "icon"),
    (64, "icon"),
    (96, "icon"),
    (128, "icon"),
    (256, "icon"),
];

fn main() {
    let images: Vec<Vec<u8>> = SIZES
        .iter()
        .map(|&(size, drawing)| {
            let svg = std::fs::read(format!("assets/{drawing}.svg")).expect("run from the repository root");
            let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).unwrap();
            let mut pixmap = tiny_skia::Pixmap::new(size, size).unwrap();
            let scale = size as f32 / tree.size().width();
            resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
            pixmap.encode_png().unwrap()
        })
        .collect();

    // ICONDIR, one ICONDIRENTRY per image, then the PNG payloads. A size byte of 0 means 256.
    let mut ico = Vec::new();
    ico.extend([0u16, 1, images.len() as u16].iter().flat_map(|v| v.to_le_bytes()));
    let mut offset = 6 + 16 * images.len() as u32;
    for (&(size, _), png) in SIZES.iter().zip(&images) {
        ico.extend([size as u8, size as u8, 0, 0]);
        ico.extend([1u16, 32].iter().flat_map(|v| v.to_le_bytes()));
        ico.extend([png.len() as u32, offset].iter().flat_map(|v| v.to_le_bytes()));
        offset += png.len() as u32;
    }
    ico.extend(images.concat());
    std::fs::write("assets/icon.ico", ico).unwrap();
    println!("assets/icon.ico: {} sizes, {offset} bytes", images.len());
}
