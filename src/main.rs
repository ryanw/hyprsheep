//! hyprsheep - a desktop sheep for Hyprland, after the 1995 eSheep.

mod anim;
mod engine;
mod expr;
mod overlay;
mod sprites;

/// The sprite sheet is baked into the binary so the sheep has no data files to find.
const SHEET: &[u8] = include_bytes!("../assets/esheep-sprites.png");

fn main() {
    let sheet = match sprites::Sheet::load(SHEET) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hyprsheep: could not load sprite sheet: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "hyprsheep: sheet {}x{} = {}x{} tiles of {}px",
        sheet.width,
        sheet.height,
        sheet.cols(),
        sheet.rows(),
        sprites::TILE
    );

    if let Err(e) = overlay::run(sheet) {
        eprintln!("hyprsheep: {e}");
        std::process::exit(1);
    }
}
