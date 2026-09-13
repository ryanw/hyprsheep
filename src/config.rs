//! Optional configuration, read from `~/.config/hyprsheep/config.toml`.
//!
//! Only a flat subset of TOML is understood - `key = value` pairs with string,
//! integer, boolean and string-array values - which is all the settings need.
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
    pub monitors: Monitors,
    /// Whether the sheep can be picked up with the mouse. When off, the
    /// overlay stays entirely click-through.
    pub draggable: bool,
    /// An alternative pet file in the same XML format. The sprite sheet is
    /// taken from the file itself.
    pub pet: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Config { sheep: 1, monitors: Monitors::All, draggable: true, pet: None }
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
                ("draggable", Value::Bool(b)) => cfg.draggable = *b,
                ("monitors", Value::Str(s)) if s == "all" => cfg.monitors = Monitors::All,
                ("monitors", Value::List(l)) => cfg.monitors = Monitors::Only(l.clone()),
                ("monitors", Value::Str(s)) => cfg.monitors = Monitors::Only(vec![s.clone()]),
                ("pet", Value::Str(s)) => cfg.pet = Some(expand_tilde(s, home.as_deref())),
                _ => eprintln!("hyprsheep: {}: ignoring `{key}`", from.display()),
            }
        }
        cfg
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Value {
    Str(String),
    Int(i64),
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
            draggable = false
            monitors = ["eDP-1", "HDMI-A-1"]
            pet = "/tmp/green.xml"
            "#,
        );
        assert_eq!(c.sheep, 3);
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
