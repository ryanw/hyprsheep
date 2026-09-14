//! hyprsheep - a desktop sheep for Hyprland, after the 1995 eSheep.

mod anim;
mod config;
mod engine;
mod expr;
mod hypr;
mod overlay;
mod sprites;

use anim::Pet;
use sprites::Sheet;

/// The sheep ships inside the binary, so there is nothing to install. Its
/// animation file has the usual base64 payload stripped, since the sheet sits
/// beside it rather than inside it.
const SHEET: &[u8] = include_bytes!("../assets/esheep-sprites.png");
const PET: &str = include_str!("../assets/animations.xml");

const USAGE: &str = "\
hyprsheep - a desktop sheep for Hyprland

usage: hyprsheep [options]

  -h, --help            show this message
  -V, --version         show the version

  --sheep N             how many sheep to keep on screen
  --scale N             how big to draw them, e.g. 2 or 0.5
  --monitors LIST       \"all\", or names: --monitors eDP-1,HDMI-A-1
  --draggable[=BOOL]    whether the sheep can be picked up with the mouse
  --no-draggable        the same as --draggable=false
  --pet PATH            an alternative pet file in the eSheep XML format
  --trace[=BOOL]        log every animation change and where the sheep is
  --no-trace            the same as --trace=false

The same settings can be put in ~/.config/hyprsheep/config.toml, one per line,
as `sheep = 1` or `monitors = [\"eDP-1\"]`. Anything given on the command
line wins over the file.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return print!("{USAGE}");
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        return println!("hyprsheep {}", env!("CARGO_PKG_VERSION"));
    }

    // The file sets the defaults; the command line overrides them.
    let mut cfg = config::Config::load();
    if let Err(e) = cfg.apply_args(args) {
        eprintln!("hyprsheep: {e}\n\n{USAGE}");
        std::process::exit(2);
    }
    let cfg = cfg;
    let (pet, sheet) = match load_pet(&cfg) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("hyprsheep: {e}");
            std::process::exit(1);
        }
    };

    println!(
        "hyprsheep: {}x{} tiles of {}px, {} animations",
        pet.tiles_x,
        pet.tiles_y,
        sheet.width / pet.tiles_x.max(1),
        pet.animations.len()
    );

    if let Err(e) = overlay::run(sheet, pet, cfg) {
        eprintln!("hyprsheep: {e}");
        std::process::exit(1);
    }
}

/// Load the configured pet, or the built-in sheep.
///
/// A pet file is normally self-contained, carrying its own sprite sheet, so an
/// external one supplies both halves.
fn load_pet(cfg: &config::Config) -> Result<(Pet, Sheet), String> {
    let Some(path) = &cfg.pet else {
        let sheet = Sheet::load(SHEET).map_err(|e| format!("built-in sprite sheet: {e}"))?;
        let pet = Pet::parse(PET).map_err(|e| format!("built-in animations: {e}"))?;
        return Ok((pet, sheet));
    };

    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read pet file {}: {e}", path.display()))?;
    let mut pet =
        Pet::parse(&text).map_err(|e| format!("pet file {}: {e}", path.display()))?;
    let png = pet
        .png
        .take()
        .ok_or_else(|| format!("pet file {} has no <png> sprite sheet", path.display()))?;
    let sheet = Sheet::load(&png)
        .map_err(|e| format!("sprite sheet in {}: {e}", path.display()))?;

    // Sheets in the wild do not always divide evenly into their declared
    // grid, so tile size is a fraction rather than a whole number of pixels.
    // That is what the reference does too; it is only worth a word of warning.
    if sheet.width % pet.tiles_x != 0 || sheet.height % pet.tiles_y != 0 {
        eprintln!(
            "hyprsheep: {}: {}x{} sheet does not divide evenly into {}x{} tiles",
            path.display(),
            sheet.width,
            sheet.height,
            pet.tiles_x,
            pet.tiles_y
        );
    }
    Ok((pet, sheet))
}
