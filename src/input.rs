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

// evdev key codes (Linux input-event-codes) carried by CTerm physical key
// reports — layout-independent physical positions. The quit keys aren't GB
// buttons, so they're matched directly in the game loop.
pub const EVDEV_ESC: u16 = 1;
pub const EVDEV_Q: u16 = 16;
const EVDEV_ENTER: u16 = 28;
const EVDEV_Z: u16 = 44;
const EVDEV_X: u16 = 45;
const EVDEV_SPACE: u16 = 57;
const EVDEV_UP: u16 = 103;
const EVDEV_LEFT: u16 = 105;
const EVDEV_RIGHT: u16 = 106;
const EVDEV_DOWN: u16 = 108;

/// Map a CTerm physical-key evdev code to a Game Boy button — same layout as the
/// translated mapping (Z=A, X=B, Enter=Start, Space=Select, arrows=D-pad).
pub fn evdev_to_button(code: u16) -> Option<Button> {
    match code {
        EVDEV_UP => Some(Button::Up),
        EVDEV_DOWN => Some(Button::Down),
        EVDEV_LEFT => Some(Button::Left),
        EVDEV_RIGHT => Some(Button::Right),
        EVDEV_Z => Some(Button::A),
        EVDEV_X => Some(Button::B),
        EVDEV_ENTER => Some(Button::Start),
        EVDEV_SPACE => Some(Button::Select),
        _ => None,
    }
}
