use std::io;
use std::path::PathBuf;

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
    /// Persisted sound mode: "off" | "ansi" | "apc".
    pub sound: Option<String>,
    /// Persisted game-list filter: "all" | "gb" | "gbc".
    pub filter: Option<String>,
    /// Persisted screen-size mode: "auto" | "best".
    pub screen: Option<String>,
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
        } else if let Some(val) = line.strip_prefix("sound=") {
            cfg.sound = Some(val.trim().to_lowercase());
        } else if let Some(val) = line.strip_prefix("audio=") {
            // Back-compat: the old on/off audio toggle maps to ansi/off.
            if cfg.sound.is_none() {
                cfg.sound = Some(
                    if val.trim().eq_ignore_ascii_case("on") { "ansi" } else { "off" }.to_string(),
                );
            }
        } else if let Some(val) = line.strip_prefix("filter=") {
            cfg.filter = Some(val.trim().to_lowercase());
        } else if let Some(val) = line.strip_prefix("screen=") {
            cfg.screen = Some(val.trim().to_lowercase());
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
    if let Some(ref s) = cfg.sound {
        contents.push_str(&format!("sound={}\n", s));
    }
    if let Some(ref f) = cfg.filter {
        contents.push_str(&format!("filter={}\n", f));
    }
    if let Some(ref s) = cfg.screen {
        contents.push_str(&format!("screen={}\n", s));
    }
    std::fs::write(file, contents)?;
    Ok(())
}

/// Sysop-level door defaults, read from `lameboy.ini` next to the binary (or the
/// working directory). These are the settings a sysop sets once instead of
/// piling them onto the door command line; an explicit command-line flag always
/// overrides the ini. Per-call values (the DOOR32 dropfile, the user id) are
/// never here — the BBS supplies those at launch.
///
/// Example `lameboy.ini`:
///   roms = roms
///   fps = 20
///   ansi_music = true
///   audio_chunk_ms = 40
///   audio_rate = 22050
#[derive(Default)]
pub struct DoorIni {
    pub roms: Option<String>,
    pub fps: Option<f64>,
    pub ansi_music: Option<bool>,
    /// Smallest APC audio clip emitted (ms); also the drop/skip granularity.
    pub audio_chunk_ms: Option<u32>,
    /// APC audio output sample rate (Hz); lower = less bandwidth on the link.
    pub audio_rate: Option<u32>,
}

/// APC audio stream tuning, sourced from `lameboy.ini` (sysop) with built-in
/// defaults. `chunk_ms` is the min clip / drop granularity; `rate` is the output
/// sample rate (the bandwidth lever — lower keeps the link from saturating). The
/// stream self-corrects latency against wall-clock, so there is no cushion knob.
#[derive(Clone, Copy)]
pub struct ApcTuning {
    pub chunk_ms: u32,
    pub rate: u32,
}

impl ApcTuning {
    pub const DEFAULT: ApcTuning = ApcTuning { chunk_ms: 40, rate: 22050 };
}

impl Default for ApcTuning {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl DoorIni {
    /// Resolve APC tuning from the ini, falling back to the built-in defaults.
    pub fn apc_tuning(&self) -> ApcTuning {
        ApcTuning {
            chunk_ms: self.audio_chunk_ms.unwrap_or(ApcTuning::DEFAULT.chunk_ms),
            rate: self.audio_rate.unwrap_or(ApcTuning::DEFAULT.rate),
        }
    }
}

/// Load `lameboy.ini` if present (working dir first, then the binary's dir).
pub fn load_door_ini() -> DoorIni {
    let mut paths: Vec<PathBuf> = vec![PathBuf::from("lameboy.ini")];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("lameboy.ini"));
        }
    }
    for p in paths {
        if let Ok(text) = std::fs::read_to_string(&p) {
            return parse_door_ini(&text);
        }
    }
    DoorIni::default()
}

fn parse_door_ini(text: &str) -> DoorIni {
    let mut ini = DoorIni::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else { continue };
        let val = val.trim();
        match key.trim().to_lowercase().as_str() {
            "roms" => {
                if !val.is_empty() {
                    ini.roms = Some(val.to_string());
                }
            }
            "fps" => ini.fps = val.parse::<f64>().ok().map(|f| f.clamp(5.0, 60.0)),
            "ansi_music" | "ansimusic" | "music" => {
                ini.ansi_music = Some(matches!(
                    val.to_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                ));
            }
            "audio_chunk_ms" | "chunk_ms" => {
                ini.audio_chunk_ms = val.parse::<u32>().ok().map(|v| v.clamp(10, 250));
            }
            "audio_rate" | "rate" | "audio_hz" => {
                ini.audio_rate = val.parse::<u32>().ok().map(|v| v.clamp(5512, 44100));
            }
            _ => {}
        }
    }
    ini
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn door_ini_parses_keys_and_ignores_comments() {
        let i = parse_door_ini("; sysop defaults\nroms = /bbs/gb/roms\nfps = 30\nansi_music = off\n");
        assert_eq!(i.roms.as_deref(), Some("/bbs/gb/roms"));
        assert_eq!(i.fps, Some(30.0));
        assert_eq!(i.ansi_music, Some(false));
    }

    #[test]
    fn door_ini_fps_is_clamped() {
        assert_eq!(parse_door_ini("fps=999").fps, Some(60.0));
        assert_eq!(parse_door_ini("fps=1").fps, Some(5.0));
    }

    #[test]
    fn door_ini_audio_tuning_parses_and_clamps() {
        let i = parse_door_ini("audio_chunk_ms = 60\naudio_rate = 11025\n");
        assert_eq!(i.audio_chunk_ms, Some(60));
        assert_eq!(i.audio_rate, Some(11025));
        // Out-of-range values clamp to the supported window.
        assert_eq!(parse_door_ini("audio_chunk_ms=1").audio_chunk_ms, Some(10));
        assert_eq!(parse_door_ini("audio_rate=999999").audio_rate, Some(44100));
        // Unset -> built-in defaults via apc_tuning().
        let d = DoorIni::default().apc_tuning();
        assert_eq!((d.chunk_ms, d.rate), (40, 22050));
    }
}
