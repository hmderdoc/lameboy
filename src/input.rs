use crossterm::event::KeyCode;
use gameboy_core::Button;

/// Map keyboard keys to GameBoy buttons
pub fn map_key_to_button(key: KeyCode) -> Option<Button> {
    match key {
        KeyCode::Up => Some(Button::Up),
        KeyCode::Down => Some(Button::Down),
        KeyCode::Left => Some(Button::Left),
        KeyCode::Right => Some(Button::Right),
        KeyCode::Char('z') | KeyCode::Char('Z') => Some(Button::A),
        KeyCode::Char('x') | KeyCode::Char('X') => Some(Button::B),
        KeyCode::Enter => Some(Button::Start),
        KeyCode::Char(' ') => Some(Button::Select),
        _ => None,
    }
}

