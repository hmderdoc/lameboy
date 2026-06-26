use gameboy_core::Gameboy;
use std::io;
use std::path::{Path, PathBuf};

/// Sanitize a user key so it is safe as a directory-name component.
fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Get the saves directory path (hidden .saves folder next to the ROM).
///
/// When `user` is set, saves are isolated under `.saves/<user>/` so each BBS
/// caller keeps their own battery saves; without a user they live directly in
/// `.saves/` (the single-player / pre-existing layout).
fn get_saves_dir(rom_path: &Path, user: Option<&str>) -> PathBuf {
    let base = rom_path
        .parent()
        .map(|p| p.join(".saves"))
        .unwrap_or_else(|| PathBuf::from(".saves"));
    match user {
        Some(u) if !u.is_empty() => base.join(sanitize(u)),
        _ => base,
    }
}

/// Get the save file path for a ROM (in .saves/ directory with .sav extension)
pub fn get_save_path(rom_path: &Path, user: Option<&str>) -> PathBuf {
    let saves_dir = get_saves_dir(rom_path, user);
    let rom_name = rom_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    saves_dir.join(format!("{}.sav", rom_name))
}

/// Load save data from disk into the gameboy if it exists
pub fn load_save(gameboy: &mut Gameboy, rom_path: &Path, user: Option<&str>) -> io::Result<bool> {
    let save_path = get_save_path(rom_path, user);

    // Check if this cartridge supports battery saves
    if !gameboy.get_cartridge().has_battery() {
        return Ok(false);
    }

    if save_path.exists() {
        let save_data = std::fs::read(&save_path)?;
        gameboy.get_cartridge_mut().set_ram(save_data);
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Save the cartridge RAM to disk
pub fn save_game(gameboy: &Gameboy, rom_path: &Path, user: Option<&str>) -> io::Result<bool> {
    // Check if this cartridge supports battery saves
    if !gameboy.get_cartridge().has_battery() {
        return Ok(false);
    }

    let save_path = get_save_path(rom_path, user);
    
    // Create the .saves directory if it doesn't exist
    if let Some(saves_dir) = save_path.parent() {
        std::fs::create_dir_all(saves_dir)?;
    }
    
    let ram = gameboy.get_cartridge().get_ram();
    std::fs::write(&save_path, ram)?;
    Ok(true)
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_path_is_isolated_per_user() {
        let rom = Path::new("/games/roms/Tetris.gb");
        // No user => shared .saves/ (pre-existing layout)
        assert_eq!(get_save_path(rom, None), Path::new("/games/roms/.saves/Tetris.sav"));
        // With user => own subdirectory
        assert_eq!(get_save_path(rom, Some("0007")), Path::new("/games/roms/.saves/0007/Tetris.sav"));
        // Unsafe chars in the key are folded to '_'
        assert_eq!(get_save_path(rom, Some("a/b 7")), Path::new("/games/roms/.saves/a_b_7/Tetris.sav"));
    }
}
