use std::io;
use std::path::{Path, PathBuf};

/// Returns `~/.config/terminal_gameboy/` using $HOME; None if HOME is unset.
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("terminal_gameboy")
    })
}

/// Sanitize a user key so it is safe to use as a filename component
/// (keep alphanumerics, fold everything else to '_').
fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Returns the path to the config file. When `user` is set, preferences are
/// stored in a per-user file (`config-<user>`) so each BBS caller keeps their
/// own render/audio/filter choices; the door process otherwise shares one
/// $HOME across all callers.
fn config_file(user: Option<&str>) -> Option<PathBuf> {
    config_dir().map(|d| match user {
        Some(u) if !u.is_empty() => d.join(format!("config-{}", sanitize(u))),
        _ => d.join("config"),
    })
}

#[derive(Default)]
pub struct Config {
    pub roms_dir: Option<PathBuf>,
    /// Persisted render mode: Some(true) = Block, Some(false) = ASCII.
    pub render_block: Option<bool>,
    pub audio_enabled: Option<bool>,
    /// Persisted game-list filter: "all" | "gb" | "gbc".
    pub filter: Option<String>,
}

/// Load config from disk. Returns defaults on any error or if the file doesn't
/// exist yet.
pub fn load(user: Option<&str>) -> Config {
    let Some(path) = config_file(user) else {
        return Config::default();
    };

    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Config::default();
    };

    let mut cfg = Config::default();
    for line in contents.lines() {
        if let Some(val) = line.strip_prefix("roms_dir=") {
            let p = PathBuf::from(val.trim());
            if p.is_dir() {
                cfg.roms_dir = Some(p);
            }
        } else if let Some(val) = line.strip_prefix("render=") {
            cfg.render_block = Some(val.trim().eq_ignore_ascii_case("block"));
        } else if let Some(val) = line.strip_prefix("audio=") {
            cfg.audio_enabled = Some(val.trim().eq_ignore_ascii_case("on"));
        } else if let Some(val) = line.strip_prefix("filter=") {
            cfg.filter = Some(val.trim().to_lowercase());
        }
    }

    cfg
}

/// Persist config to disk, creating the config directory if needed.
pub fn save(user: Option<&str>, cfg: &Config) -> io::Result<()> {
    let Some(dir) = config_dir() else {
        return Ok(()); // No HOME set — skip silently
    };
    let Some(file) = config_file(user) else {
        return Ok(());
    };

    std::fs::create_dir_all(&dir)?;

    let mut contents = String::new();
    if let Some(ref p) = cfg.roms_dir {
        contents.push_str(&format!("roms_dir={}\n", p.display()));
    }
    if let Some(block) = cfg.render_block {
        contents.push_str(&format!("render={}\n", if block { "block" } else { "ascii" }));
    }
    if let Some(audio) = cfg.audio_enabled {
        contents.push_str(&format!("audio={}\n", if audio { "on" } else { "off" }));
    }
    if let Some(ref f) = cfg.filter {
        contents.push_str(&format!("filter={}\n", f));
    }
    std::fs::write(file, contents)?;
    Ok(())
}

/// Expand a leading `~` to the home directory. Returns None if `~` is used
/// but HOME is unset.
pub fn expand_tilde(path: &str) -> Option<PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var_os("HOME")?;
        Some(Path::new(&home).join(rest))
    } else if path == "~" {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home))
    } else {
        Some(PathBuf::from(path))
    }
}
