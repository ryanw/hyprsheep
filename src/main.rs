//! hyprsheep - a desktop sheep for Hyprland, after the 1995 eSheep.

mod anim;
mod engine;
mod expr;
mod hypr;
mod overlay;
mod sprites;

/// The sprite sheet and animation data are baked into the binary so the sheep
/// has no data files to find.
const SHEET: &[u8] = include_bytes!("../assets/esheep-sprites.png");
const PET: &str = include_str!("../assets/animations.xml");

fn main() {
    let sheet = match sprites::Sheet::load(SHEET) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hyprsheep: could not load sprite sheet: {e}");
            std::process::exit(1);
        }
    };
    let pet = match anim::Pet::parse(PET) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hyprsheep: could not load animations: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "hyprsheep: sheet {}x{}, {}x{} tiles, {} animations",
        sheet.width,
        sheet.height,
        pet.tiles_x,
        pet.tiles_y,
        pet.animations.len()
    );

    if let Err(e) = overlay::run(sheet, pet) {
        eprintln!("hyprsheep: {e}");
        std::process::exit(1);
    }
}
