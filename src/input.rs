use crate::keys::Key;
use gameboy_core::Button;

/// Map a decoded key to a Game Boy button.
pub fn map_key_to_button(key: Key) -> Option<Button> {
    match key {
        Key::Up => Some(Button::Up),
        Key::Down => Some(Button::Down),
        Key::Left => Some(Button::Left),
        Key::Right => Some(Button::Right),
        Key::Char('z') | Key::Char('Z') => Some(Button::A),
        Key::Char('x') | Key::Char('X') => Some(Button::B),
        Key::Enter => Some(Button::Start),
        Key::Char(' ') => Some(Button::Select),
        _ => None,
    }
}
