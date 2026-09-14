//! Optional configuration, read from `~/.config/hyprsheep/config.toml`.
//!
//! Only a flat subset of TOML is understood - `key = value` pairs with string,
//! number, boolean and string-array values - which is all the settings need.
//! A missing or malformed file is never fatal: bad lines are reported and the
//! default is kept, so a typo cannot leave the user without a sheep.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum Monitors {
    /// Every output.
    All,
    /// Only these connector names, e.g. `eDP-1`.
    Only(Vec<String>),
}

impl Monitors {
    pub fn allows(&self, name: &str) -> bool {
        match self {
            Monitors::All => true,
            Monitors::Only(list) => list.iter().any(|n| n == name),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// How many sheep to keep on screen.
    pub sheep: usize,
    /// How big to draw the sheep, as a multiple of the sprite's own size.
    pub scale: f64,
    pub monitors: Monitors,
    /// Whether the sheep can be picked up with the mouse. When off, the
    /// overlay stays entirely click-through.
    pub draggable: bool,
    /// An alternative pet file in the same XML format. The sprite sheet is
    /// taken from the file itself.
    pub pet: Option<PathBuf>,
    /// Log every animation change and where the sheep is.
    pub trace: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sheep: 1,
            scale: 1.0,
            monitors: Monitors::All,
            draggable: true,
            pet: None,
            trace: false,
        }
    }
}

/// `$XDG_CONFIG_HOME/hyprsheep/config.toml`, falling back to `~/.config`.
pub fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("hyprsheep").join("config.toml"))
}

impl Config {
    pub fn load() -> Config {
        let mut cfg = Config::load_file();
        // The environment variable is a convenience equivalent to --trace.
        cfg.trace |= std::env::var_os("HYPRSHEEP_TRACE").is_some();
        cfg
    }

    fn load_file() -> Config {
        let Some(p) = path() else { return Config::default() };
        match std::fs::read_to_string(&p) {
            Ok(text) => Config::parse(&text, &p),
            // No config file at all is the normal case, not a problem.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => {
                eprintln!("hyprsheep: could not read {}: {e}", p.display());
                Config::default()
            }
        }
    }

    pub fn parse(text: &str, from: &Path) -> Config {
        let mut cfg = Config::default();
        let mut seen: HashMap<String, Value> = HashMap::new();
        let home = std::env::var_os("HOME").map(PathBuf::from);

        for (n, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, rest)) = line.split_once('=') else {
                eprintln!("hyprsheep: {}:{}: expected `key = value`", from.display(), n + 1);
                continue;
            };
            match parse_value(rest.trim()) {
                Some(v) => {
                    seen.insert(key.trim().to_string(), v);
                }
                None => eprintln!(
                    "hyprsheep: {}:{}: could not read value for `{}`",
                    from.display(),
                    n + 1,
                    key.trim()
                ),
            }
        }

        for (key, value) in &seen {
            match (key.as_str(), value) {
                ("sheep", Value::Int(n)) if *n >= 1 => cfg.sheep = *n as usize,
                ("scale", Value::Float(f)) if scale_ok(*f) => cfg.scale = *f,
                ("scale", Value::Int(n)) if scale_ok(*n as f64) => cfg.scale = *n as f64,
                ("draggable", Value::Bool(b)) => cfg.draggable = *b,
                ("monitors", Value::Str(s)) if s == "all" => cfg.monitors = Monitors::All,
                ("monitors", Value::List(l)) => cfg.monitors = Monitors::Only(l.clone()),
                ("monitors", Value::Str(s)) => cfg.monitors = Monitors::Only(vec![s.clone()]),
                ("pet", Value::Str(s)) => cfg.pet = Some(expand_tilde(s, home.as_deref())),
                ("trace", Value::Bool(b)) => cfg.trace = *b,
                _ => eprintln!("hyprsheep: {}: ignoring `{key}`", from.display()),
            }
        }
        cfg
    }
}

/// Every setting can also be given on the command line, where it overrides
/// whatever the file said.
impl Config {
    pub fn apply_args<I: IntoIterator<Item = String>>(&mut self, args: I) -> Result<(), String> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut args = args.into_iter().peekable();

        while let Some(arg) = args.next() {
            // Both `--key value` and `--key=value` are accepted.
            let (key, inline) = match arg.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (arg.clone(), None),
            };
            let mut value = || match inline.clone() {
                Some(v) => Ok(v),
                None => args.next().ok_or(format!("{key} needs a value")),
            };

            match key.as_str() {
                "--sheep" => {
                    let v = value()?;
                    self.sheep = v
                        .parse()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or(format!("--sheep wants a number of 1 or more, not {v:?}"))?;
                }
                "--scale" => {
                    let v = value()?;
                    self.scale = v.parse().ok().filter(|f| scale_ok(*f)).ok_or(format!(
                        "--scale wants a multiplier between {SCALE_MIN} and {SCALE_MAX}, not {v:?}"
                    ))?;
                }
                "--monitors" => {
                    let v = value()?;
                    self.monitors = if v == "all" {
                        Monitors::All
                    } else {
                        let names: Vec<String> = v
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect();
                        if names.is_empty() {
                            return Err("--monitors wants \"all\" or a list of names".into());
                        }
                        Monitors::Only(names)
                    };
                }
                "--pet" => self.pet = Some(expand_tilde(&value()?, home.as_deref())),
                "--draggable" => self.draggable = flag(inline.as_deref(), &key)?,
                "--no-draggable" => self.draggable = false,
                "--trace" => self.trace = flag(inline.as_deref(), &key)?,
                "--no-trace" => self.trace = false,
                other => return Err(format!("unknown option `{other}`")),
            }
        }
        Ok(())
    }
}

/// The sheep may be shrunk or blown up, but not to nothing or to absurdity:
/// below this it is a speck, above it a wall of wool.
const SCALE_MIN: f64 = 0.1;
const SCALE_MAX: f64 = 20.0;

fn scale_ok(f: f64) -> bool {
    f.is_finite() && (SCALE_MIN..=SCALE_MAX).contains(&f)
}

/// A boolean flag: bare means on, or an explicit `=true`/`=false`.
fn flag(inline: Option<&str>, key: &str) -> Result<bool, String> {
    match inline {
        None => Ok(true),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(v) => Err(format!("{key} wants true or false, not {v:?}")),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<String>),
}

/// Drop a trailing `#` comment, ignoring one inside a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        Some(s[1..s.len() - 1].to_string())
    } else if !s.is_empty() && !s.contains(['"', '[', ']']) {
        // Tolerate an unquoted bare word, which is what people actually type.
        Some(s.to_string())
    } else {
        None
    }
}

fn parse_value(s: &str) -> Option<Value> {
    if s == "true" {
        return Some(Value::Bool(true));
    }
    if s == "false" {
        return Some(Value::Bool(false));
    }
    if let Ok(n) = s.parse::<i64>() {
        return Some(Value::Int(n));
    }
    // Only a number written with a point or an exponent is a float; anything
    // else that parses as one (`inf`, `nan`) is not what the user meant.
    let written_as_a_number = s.contains(['.', 'e', 'E']);
    if let Some(f) = s.parse::<f64>().ok().filter(|f| f.is_finite() && written_as_a_number) {
        return Some(Value::Float(f));
    }
    if let Some(inner) = s.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let items: Option<Vec<String>> = inner
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(unquote)
            .collect();
        return items.map(Value::List);
    }
    unquote(s).map(Value::Str)
}

fn expand_tilde(s: &str, home: Option<&Path>) -> PathBuf {
    match (s.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        Config::parse(text, Path::new("test"))
    }

    #[test]
    fn an_empty_file_is_the_default() {
        assert_eq!(parse(""), Config::default());
        assert_eq!(parse("\n\n# just a comment\n"), Config::default());
    }

    #[test]
    fn reads_every_setting() {
        let c = parse(
            r#"
            sheep = 3
            scale = 1.5
            draggable = false
            monitors = ["eDP-1", "HDMI-A-1"]
            pet = "/tmp/green.xml"
            "#,
        );
        assert_eq!(c.sheep, 3);
        assert_eq!(c.scale, 1.5);
        assert!(!c.draggable);
        assert_eq!(
            c.monitors,
            Monitors::Only(vec!["eDP-1".into(), "HDMI-A-1".into()])
        );
        assert_eq!(c.pet, Some(PathBuf::from("/tmp/green.xml")));
    }

    #[test]
    fn monitors_accepts_all_or_a_single_name() {
        assert_eq!(parse("monitors = \"all\"").monitors, Monitors::All);
        assert_eq!(parse("monitors = all").monitors, Monitors::All);
        assert_eq!(
            parse("monitors = \"eDP-1\"").monitors,
            Monitors::Only(vec!["eDP-1".into()])
        );
        assert!(Monitors::All.allows("anything"));
        assert!(Monitors::Only(vec!["eDP-1".into()]).allows("eDP-1"));
        assert!(!Monitors::Only(vec!["eDP-1".into()]).allows("HDMI-A-1"));
    }

    #[test]
    fn comments_and_bare_words_are_tolerated() {
        let c = parse("sheep = 2 # how many\ndraggable = true\n");
        assert_eq!(c.sheep, 2);
        assert!(c.draggable);
        // A `#` inside a quoted value is not a comment.
        assert_eq!(parse(r#"pet = "/tmp/a#b.xml""#).pet, Some(PathBuf::from("/tmp/a#b.xml")));
    }

    #[test]
    fn bad_input_falls_back_rather_than_failing() {
        // Nonsense lines, unknown keys and wrong types all keep the default.
        let c = parse("nonsense\nsheep = lots\nunknown = 1\nsheep = 0\n");
        assert_eq!(c, Config::default());
    }

    fn args(list: &[&str]) -> Result<Config, String> {
        let mut c = Config::default();
        c.apply_args(list.iter().map(|s| s.to_string()))?;
        Ok(c)
    }

    #[test]
    fn every_setting_has_a_command_line_form() {
        let c = args(&["--sheep", "4", "--scale", "2", "--monitors", "eDP-1,HDMI-A-1", "--no-draggable"])
            .unwrap();
        assert_eq!(c.sheep, 4);
        assert_eq!(c.scale, 2.0);
        assert_eq!(c.monitors, Monitors::Only(vec!["eDP-1".into(), "HDMI-A-1".into()]));
        assert!(!c.draggable);

        // Everything the file understands is also an option.
        assert_eq!(args(&["--monitors", "all"]).unwrap().monitors, Monitors::All);
        assert_eq!(args(&["--pet", "/tmp/g.xml"]).unwrap().pet, Some(PathBuf::from("/tmp/g.xml")));
        assert!(args(&["--trace"]).unwrap().trace);
    }

    #[test]
    fn both_spellings_work() {
        assert_eq!(args(&["--sheep=7"]).unwrap().sheep, 7);
        assert_eq!(args(&["--sheep", "7"]).unwrap().sheep, 7);
        assert_eq!(args(&["--pet=/a/b.xml"]).unwrap().pet, Some(PathBuf::from("/a/b.xml")));
        // Booleans are bare, negated, or explicit.
        assert!(args(&["--draggable"]).unwrap().draggable);
        assert!(!args(&["--no-draggable"]).unwrap().draggable);
        assert!(!args(&["--draggable=false"]).unwrap().draggable);
        assert!(args(&["--draggable=true"]).unwrap().draggable);
    }

    #[test]
    fn the_command_line_wins_over_the_file() {
        let mut c = parse("sheep = 2\ndraggable = true\nmonitors = [\"eDP-1\"]\n");
        c.apply_args(["--sheep=9", "--no-draggable"].iter().map(|s| s.to_string())).unwrap();
        assert_eq!(c.sheep, 9);
        assert!(!c.draggable);
        // Settings not given on the command line keep the file's value.
        assert_eq!(c.monitors, Monitors::Only(vec!["eDP-1".into()]));
    }

    #[test]
    fn bad_options_are_rejected_with_a_reason() {
        // Unlike the config file, a bad option is worth stopping for: the user
        // is standing right there and can fix it.
        for bad in [
            vec!["--nonsense"],
            vec!["--sheep"],
            vec!["--sheep", "lots"],
            vec!["--sheep", "0"],
            vec!["--scale"],
            vec!["--scale", "big"],
            vec!["--scale", "0"],
            vec!["--scale", "-2"],
            vec!["--scale", "1000"],
            vec!["--scale", "inf"],
            vec!["--draggable=maybe"],
            vec!["--monitors", ""],
            vec!["--pet"],
        ] {
            assert!(args(&bad).is_err(), "{bad:?} should have been rejected");
        }
    }

    #[test]
    fn trace_can_be_set_either_way() {
        assert!(!Config::default().trace);
        assert!(parse("trace = true").trace);
        assert!(!args(&["--trace", "--no-trace"]).unwrap().trace);
    }

    #[test]
    fn scale_takes_whole_or_fractional_sizes() {
        assert_eq!(Config::default().scale, 1.0);
        assert_eq!(parse("scale = 2").scale, 2.0);
        assert_eq!(parse("scale = 0.5").scale, 0.5);
        assert_eq!(args(&["--scale=0.25"]).unwrap().scale, 0.25);
        // Out of range, or not a number at all, keeps the default in a file.
        assert_eq!(parse("scale = 0").scale, 1.0);
        assert_eq!(parse("scale = 100.0").scale, 1.0);
        assert_eq!(parse("scale = huge").scale, 1.0);
        assert_eq!(parse("scale = nan").scale, 1.0);
    }

    #[test]
    fn tilde_is_expanded() {
        let home = PathBuf::from("/home/someone");
        assert_eq!(
            expand_tilde("~/pets/green.xml", Some(&home)),
            PathBuf::from("/home/someone/pets/green.xml")
        );
        // Without a home directory the path is left alone rather than mangled.
        assert_eq!(expand_tilde("~/pets/green.xml", None), PathBuf::from("~/pets/green.xml"));
        assert_eq!(expand_tilde("/abs/path.xml", Some(&home)), PathBuf::from("/abs/path.xml"));
    }
}
