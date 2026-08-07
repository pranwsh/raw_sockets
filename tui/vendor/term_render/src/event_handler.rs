//! Event handling utilities for terminal input parsing.
//!
//! This module provides the `KeyParser` struct and related enums for parsing and tracking
//! keyboard and mouse events from terminal input, including support for escape codes,
//! mouse scrolling, and key modifiers.
//!
//! # Enums
//! - `KeyModifiers`: Represents keyboard modifier keys (Shift, Command, Option, Control).
//! - `KeyCode`: Represents supported key codes (Delete, Tab, Arrow keys, Return, Escape).
//! - `MouseEventType`: Represents mouse event types (Null, Left, Right, Middle, Down, Up).
//! - `MouseState`: Represents mouse button states (Release, Press, Hold, Null).
//!
//! # Structs
//! - `MouseEvent`: Stores mouse event type, position, and state.
//! - `KeyParser`: Tracks key events, modifiers, mouse events, and scroll events.
//!
//! # Features
//! - Parsing of standard and custom escape codes for keyboard and mouse.
//! - Mouse scroll event accumulation and averaging.
//! - Modifier key tracking for both keyboard and mouse events.
//! - Utility functions to enable/disable mouse capture in the terminal.
//!
//! # Usage
//! Implement the `Perform` trait for `KeyParser` to handle terminal input bytes and escape codes.
//! Use `KeyParser` methods to query for specific key, character, or modifier events.
//!
//! # Example
//! ```rust
//! let mut parser = KeyParser::new();
//! // Feed input bytes to parser via vte::Parser
//! // Query events:
//! if parser.contains_key_code(KeyCode::Return) { /* ... */ }
//! if parser.contains_modifier(KeyModifiers::Shift) { /* ... */ }
//! ```
#![allow(dead_code)]

use std::io::Write;
use vte::Perform;

// constants for tracking mouse scrolling
const SCROLL_SENSITIVITY: f64 = 0.05;
const SCROLL_LOG_TIME: f64 = 0.75;

/// A representation of keyboard modifier keys.
/// Used to track the state of modifier keys during key events.
#[repr(u8)]
#[derive(PartialEq, Eq, Debug, Default)]
pub enum KeyModifiers {
    Shift,
    #[default] Command,
    Option,
    Control,
}

/// A set of special keycodes that aren't typical characters.
/// Used to identify specific key events in terminal input.
#[repr(u8)]
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
pub enum KeyCode {
    Delete,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Return,
    Escape,
}

/// Different types of mouse events that can be detected.
#[repr(u8)]
#[derive(PartialEq, Eq, Debug, Clone, Default)]
pub enum MouseEventType {
    #[default] Null,
    Left,
    Right,
    Middle,
    Down,
    Up,
}

/// Different states of mouse buttons during events.
#[repr(u8)]
#[derive(PartialEq, Eq, Debug, Clone, Default)]
pub enum MouseState {
    Release,
    Press,
    Hold,
    #[default] Null,
}

/// A structure representing a mouse event, including its type, position, and state.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct MouseEvent {
    pub event_type: MouseEventType,
    pub position: (u16, u16),
    pub state: MouseState,
}

/// A parser for terminal input that tracks key events, modifiers, mouse events, and scroll events.
/// Implements the `vte::Perform` trait to handle input bytes and escape codes.
/// This is used internally within the lib.rs App, and as such rarely needs to be used directly.
#[derive(Default)]
pub struct KeyParser {
    pub key_modifiers: Vec <KeyModifiers>,
    pub key_events: std::collections::HashMap <KeyCode, bool>,
    pub char_events: Vec <char>,
    pub in_escape_seq: bool,
    pub bytes: usize,
    pub mouse_event: Option <MouseEvent>,
    pub mouse_modifiers: Vec <KeyModifiers>,
    pub last_press: u128,
    pub scroll_events: Vec <(std::time::SystemTime, i8)>,  // the sign is the direction
    pub scroll_accumulate: f64,
}

impl KeyParser {
    /// Creates a new `KeyParser` instance with default values.
    pub fn new () -> Self {
        KeyParser {
            key_events: std::collections::HashMap::from([
                (KeyCode::Delete, false),
                (KeyCode::Tab, false),
                (KeyCode::Left, false),
                (KeyCode::Right, false),
                (KeyCode::Up, false),
                (KeyCode::Down, false),
                (KeyCode::Return, false),
                (KeyCode::Escape, false),
            ]),
            key_modifiers: vec!(),
            char_events: vec!(),
            in_escape_seq: false,
            bytes: 0,
            mouse_event: None,
            mouse_modifiers: vec!(),
            last_press: 0,
            scroll_events: vec![],
            scroll_accumulate: 0.0,
        }
    }

    // tracking a log of scroll events to average them out over a duration of time
    /// Handles a scroll event by accumulating its direction and updating the average scroll value.
    /// This method records the time and direction of the scroll event, then calls `update_scroll
    fn scroll (&mut self, sign: i8) {
        let time = std::time::SystemTime::now();
        if self.scroll_accumulate.is_sign_negative() != sign.is_negative(){
            self.scroll_events.clear();  // so on sign flip it doesn't do weird things
        }
        self.scroll_events.push((time, sign));
        self.update_scroll();
    }

    /// Updates the average scroll value based on recent scroll events within a defined time window.
    /// This method filters out old scroll events and calculates the average scroll direction,
    /// applying sensitivity and time scaling to smooth out the scroll input.
    fn update_scroll(&mut self) {
        let time = std::time::SystemTime::now();
        let mut valid = vec![];
        let mut avg = 0.0;
        for (other_time, other_sign) in &self.scroll_events {
            // 0.000001 is the conversion rate from micro seconds to seconds
            let duration = time.duration_since(*other_time).unwrap_or_default().as_secs_f64();
            if duration < SCROLL_LOG_TIME {
                avg += *other_sign as f64; valid.push((*other_time, *other_sign));
            }
        }
        avg *= SCROLL_SENSITIVITY / SCROLL_LOG_TIME;
        self.scroll_accumulate = avg;
        self.scroll_events = valid;
    }

    /// Clears all tracked events and resets the parser state.
    /// This includes character events, key modifiers, mouse modifiers, key events,
    /// and resets the escape sequence flag. It also updates the scroll state and
    /// adjusts the mouse event state if necessary.
    /// *This should be, and only be, called after all key events have been responded to
    /// (such as after updating each updatable entity). However, this is already called
    /// internally and shouldn't ever need to be manually called*
    pub fn clear_events (&mut self) {
        self.char_events.clear();
        self.key_modifiers.clear();
        self.mouse_modifiers.clear();
        self.key_events.clear();
        self.in_escape_seq = false;
        self.update_scroll();

        if let Some(event) = &mut self.mouse_event {
            match event.state {
                MouseState::Press => {
                    event.state = MouseState::Hold;
                },
                MouseState::Hold if matches!(event.event_type, MouseEventType::Down | MouseEventType::Up) => {
                    event.state = MouseState::Release;
                },
                MouseState::Release => {
                    event.state = MouseState::Null;
                    event.event_type = MouseEventType::Null;
                },
                MouseState::Hold => {
                },
                _ => {},
            }
        }
    }

    /// Checks if a specific character event has been recorded.
    /// Returns `true` if the character is present in the recorded events, otherwise `false
    pub fn contains_char (&self, chr: char) -> bool {
        self.char_events.contains(&chr)
    }

    /// Checks if a specific key modifier is currently active.
    /// Returns `true` if the modifier is present in the active modifiers, otherwise `false
    pub fn contains_modifier (&self, modifier: KeyModifiers) -> bool {
        self.key_modifiers.contains(&modifier)
    }

    /// Checks if a specific mouse modifier is currently active.
    /// Returns `true` if the modifier is present in the active mouse modifiers, otherwise `false
    pub fn contains_mouse_modifier (&self, modifier: KeyModifiers) -> bool {
        self.mouse_modifiers.contains(&modifier)
    }

    /// Checks if a specific key code event has been recorded.
    /// Returns `true` if the key code is present in the recorded events, otherwise `false
    pub fn contains_key_code (&self, key: KeyCode) -> bool {
        *self.key_events.get(&key).unwrap_or(&false)
    }

    /// Handles mouse escape codes by parsing the provided numbers and character.
    /// This method extracts the button type, position, and modifiers from the escape code,
    /// then updates the mouse event state accordingly.
    /// It supports different mouse buttons, scroll events, and modifier keys.
    fn handle_mouse_escape_codes (&mut self, numbers: &[u16], c: char) {
        if let Some([byte, x, y]) = numbers.get(0..3) {
            let button = byte & 0b11; // Mask lowest 2 bits (button type)
            //println!("button: {}, numbers: {:?}", button, numbers);

            // adding key press modifiers
            if (byte & 32) != 0 {
                self.key_modifiers.push(KeyModifiers::Shift);
            }
            if (byte & 64) != 0 {
                self.key_modifiers.push(KeyModifiers::Option);
            }
            if (byte & 128) != 0 {
                self.key_modifiers.push(KeyModifiers::Control);
            }

            //println!("Code: {:?} / {}", numbers, c);

            let is_scroll = (byte & 64) != 0;
            let event_type = match (is_scroll, button) {
                (true, 0) => {
                    self.scroll(-1i8);
                    MouseEventType::Up
                },
                (true, 1) => {
                    self.scroll(1i8);
                    MouseEventType::Down
                },
                (false, 0) => MouseEventType::Left,
                (false, 1) => MouseEventType::Middle,
                (false, 2) => MouseEventType::Right,
                _ => MouseEventType::Null
            };

            if matches!(event_type, MouseEventType::Left) && numbers[0] == 4 {
                self.mouse_modifiers.push(KeyModifiers::Shift);
            }

            self.calculate_mouse_event_code(event_type, (*x, *y), c);
        }
    }

    /// Updates the mouse event state based on the provided event type, position, and character.
    /// This method adjusts the mouse event position if the event is a drag (hold) event
    /// and the position has changed. Otherwise, it creates a new mouse event with the given
    /// parameters.
    fn calculate_mouse_event_code (
        &mut self,
        event_type: MouseEventType,
        (x, y): (u16, u16),
        c: char
    ) {
        if let Some(event) = &mut self.mouse_event {
            if matches!(event_type, MouseEventType::Left) &&
                event.position != (x, y) &&
                matches!(event.state, MouseState::Hold) &&
                c == 'M'
            {
                event.position = (x, y);
                return;
            }
        }

        self.mouse_event = Some(MouseEvent {
            event_type,
            position: (x, y),
            state: {
                match c {
                    'M' => MouseState::Press,
                    'm' => MouseState::Release,
                    _ => MouseState::Null,
                }
            },
        });
    }

    /// Handles custom escape codes by parsing the provided numbers.
    /// This method maps specific escape code numbers to key events and modifiers,
    /// updating the key event state accordingly.
    /// It supports a variety of key combinations, including Command, Option, Shift,
    /// and Control modifiers with various keys.
    /// Look at the repository: https://github.com/AndrewDMorgan/TermEdit for a list of supported codes.
    /// 
    /// *Any custom keys need manual implementation; certain terminals like iTerm offer support to do so.
    /// This likely won't ever be used by the user, and is most an artifact of the use of this backend in TermEdit (the
    /// listed repository)*
    fn handle_custom_escape_codes (&mut self, numbers: &[u16]) {
        match numbers[1] {
            2 => {
                self.key_events.insert(KeyCode::Delete, true);
                self.key_modifiers.push(KeyModifiers::Shift);
            }
            3 => {
                self.key_events.insert(KeyCode::Delete, true);
                self.key_modifiers.push(KeyModifiers::Option);
            }
            4 => {
                self.key_events.insert(KeyCode::Left, true);
                self.key_modifiers.push(KeyModifiers::Command);
            }
            5 => {
                self.key_events.insert(KeyCode::Right, true);
                self.key_modifiers.push(KeyModifiers::Command);
            }
            6 => {
                self.key_events.insert(KeyCode::Up, true);
                self.key_modifiers.push(KeyModifiers::Command);
            }
            7 => {
                self.key_events.insert(KeyCode::Down, true);
                self.key_modifiers.push(KeyModifiers::Command);
            }
            8 => {
                self.key_events.insert(KeyCode::Delete, true);
                self.key_modifiers.push(KeyModifiers::Option);
                self.key_modifiers.push(KeyModifiers::Shift);
            }
            9 => {
                self.key_events.insert(KeyCode::Delete, true);
                self.key_modifiers.push(KeyModifiers::Command);
            }
            10 => {
                self.key_events.insert(KeyCode::Delete, true);
                self.key_modifiers.push(KeyModifiers::Command);
                self.key_modifiers.push(KeyModifiers::Shift);
            }
            11 => {
                self.key_modifiers.push(KeyModifiers::Command);
                self.char_events.push('s');  // command + s
            }
            12 => {  // lrud
                self.key_events.insert(KeyCode::Left, true);
                self.key_modifiers.push(KeyModifiers::Command);
                self.key_modifiers.push(KeyModifiers::Shift);
            }
            13 => {
                self.key_events.insert(KeyCode::Right, true);
                self.key_modifiers.push(KeyModifiers::Command);
                self.key_modifiers.push(KeyModifiers::Shift);
            }
            14 => {
                self.key_events.insert(KeyCode::Up, true);
                self.key_modifiers.push(KeyModifiers::Command);
                self.key_modifiers.push(KeyModifiers::Shift);
            }
            15 => {
                self.key_events.insert(KeyCode::Down, true);
                self.key_modifiers.push(KeyModifiers::Command);
                self.key_modifiers.push(KeyModifiers::Shift);
            }
            16 => {
                self.key_modifiers.push(KeyModifiers::Command);
                self.char_events.push('c');
            }
            17 => {
                self.key_modifiers.push(KeyModifiers::Command);
                self.char_events.push('v');
            }
            18 => {
                self.key_modifiers.push(KeyModifiers::Command);
                self.char_events.push('x');
            }
            19 => {
                self.key_modifiers.push(KeyModifiers::Command);
                self.char_events.push('f');
            }
            20 => {
                self.key_modifiers.push(KeyModifiers::Command);
                self.char_events.push('z');
            }
            21 => {
                self.key_modifiers.push(KeyModifiers::Command);
                self.key_modifiers.push(KeyModifiers::Shift);
                self.char_events.push('z');
            }
            22 => {
                self.key_events.insert(KeyCode::Tab, true);
                self.key_modifiers.push(KeyModifiers::Option);
            }
            _ => {}
        }
    }

    /// Handles control + arrow key escape codes by parsing the provided numbers and character.
    /// This method maps specific escape code characters to key events with the Control modifier,
    /// updating the key event state accordingly.
    /// It supports left, right, up, and down arrow keys combined with the Control modifier
    fn handle_control_arrows (&mut self, _numbers: &[u16], c: char) {
        match c {
            'D' => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.key_events.insert(KeyCode::Left, true);
            },
            'C' => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.key_events.insert(KeyCode::Right, true);
            },
            'A' => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.key_events.insert(KeyCode::Up, true);
            },
            'B' => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.key_events.insert(KeyCode::Down, true);
            },
            _ => {}  // control + arrows
        }
    }

    /// Handles standard escape codes by parsing the provided numbers and character.
    fn handle_standard_escape_codes (&mut self, numbers: &Vec <u16>, c: char) {
        match c as u8 {
            0x5A => {
                self.key_events.insert(KeyCode::Tab, true);
                self.key_modifiers.push(KeyModifiers::Shift);
            },
            0x44 => {
                self.key_events.insert(KeyCode::Left, true);
                if *numbers == [1, 3] {
                    self.key_modifiers.push(KeyModifiers::Option);
                } else if *numbers == [1, 2] {
                    self.key_modifiers.push(KeyModifiers::Shift);
                } else if *numbers == [1, 4] {
                    self.key_modifiers.push(KeyModifiers::Option);
                    self.key_modifiers.push(KeyModifiers::Shift);
                }
            },
            0x43 => {
                self.key_events.insert(KeyCode::Right, true);
                if *numbers == [1, 3] {
                    self.key_modifiers.push(KeyModifiers::Option);
                } else if *numbers == [1, 2] {
                    self.key_modifiers.push(KeyModifiers::Shift);
                } else if *numbers == [1, 4] {
                    self.key_modifiers.push(KeyModifiers::Option);
                    self.key_modifiers.push(KeyModifiers::Shift);
                }
            },
            0x41 => {
                self.key_events.insert(KeyCode::Up, true);
                if *numbers == [1, 3] {
                    self.key_modifiers.push(KeyModifiers::Option);
                } else if *numbers == [1, 2] {
                    self.key_modifiers.push(KeyModifiers::Shift);
                } else if *numbers == [1, 4] {
                    self.key_modifiers.push(KeyModifiers::Option);
                    self.key_modifiers.push(KeyModifiers::Shift);
                }
            },
            0x42 => {
                self.key_events.insert(KeyCode::Down, true);
                if *numbers == [1, 3] {
                    self.key_modifiers.push(KeyModifiers::Option);
                } else if *numbers == [1, 2] {
                    self.key_modifiers.push(KeyModifiers::Shift);
                } else if *numbers == [1, 4] {
                    self.key_modifiers.push(KeyModifiers::Option);
                    self.key_modifiers.push(KeyModifiers::Shift);
                }
            },
            _ => {},
        }
    }

}

/// Enables mouse capture in the terminal by sending the appropriate escape codes.
pub fn enable_mouse_capture() {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"echo -e \"\x1B[?1006h\"");
    let _ = stdout.write_all(b"\x1B[?1000h"); // Enable basic mouse mode
    let _ = stdout.write_all(b"\x1B[?1003h"); // Enable all motion events
}

/// Disables mouse capture in the terminal by sending the appropriate escape codes.
pub fn disable_mouse_capture() {
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(b"\x1B[?1000l"); // Disable mouse mode
    let _ = stdout.write_all(b"\x1B[?1003l"); // Disable motion events
}

impl KeyParser {
    /// Sets the last key press time to the current system time in milliseconds since the UNIX epoch.
    /// This is used for tracking the timing of key presses.
    pub fn set_press_time (&mut self) {
        self.last_press = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("Time went backwards...")
            .as_millis();
    }
}

impl Perform for KeyParser {
    /// Handles a printable character input.
    /// If the character is part of an escape sequence or multibyte input, it may be ignored.
    /// Special handling is included for the backspace character (0x7F) and non-graphical characters.
    /// Otherwise, the character is added to the list of character events.
    #[inline(always)]
    fn print(&mut self, chr: char) {
        //println!("char {}: '{}'", chr as u8, chr);
        if self.in_escape_seq {
            match chr as u8 {
                17 => {
                    self.char_events.push('w');
                    self.key_modifiers.push(KeyModifiers::Option);
                },
                _ => {}
            }

            return;
        }
        self.set_press_time();

        if chr as u8 == 0x7F {
            self.key_events.insert(KeyCode::Delete, true);
            return;
        }
        if !(chr.is_ascii_graphic() || chr.is_whitespace()) {  return;  }
        //println!("char {}: '{}'", chr as u8, chr);
        self.char_events.push(chr);
    }

    /// Handles a control character input.
    /// This method processes specific control characters, including escape sequences
    /// and common control key combinations (e.g., Ctrl+C, Ctrl+V).
    /// It updates the key event state and modifier keys accordingly.
    #[inline(always)]
    fn execute(&mut self, byte: u8) {
        self.set_press_time();

        // control + ...
        // 3 = c; 22 = v; 26 = z; 6 = f; 1 = a; 24 = x; 19 = s; 21 = u; r = 18
        // left ^[[1;5D right ^[[1;5C up ^[[1;5A down ^[[1;5B
        // control u and control r and necessary for undo and redo bc/
        // control + key and control + shift + key don't send unique
        // escape codes for some odd reason

        match byte {
            0x1B => {
                self.in_escape_seq = true;
            },
            0x0D => {  // return aka \n
                self.key_events.insert(KeyCode::Return, true);
            },
            0x09 => {
                self.key_events.insert(KeyCode::Tab, true);
            },// 3 = c; 22 = v; 26 = z; 6 = f; 1 = a; 24 = x; 19 = s; 21 = u; r = 18
            3 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.char_events.push('c');
            },
            22 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.char_events.push('v');
            },
            26 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.char_events.push('z');
            },
            6 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.char_events.push('f');
            },
            1 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.char_events.push('a');
            },
            24 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.char_events.push('x');
            },
            19 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.char_events.push('s');
            },
            21 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.char_events.push('u');
            },
            18 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.char_events.push('r');
            },
            0x08 => {
                self.key_modifiers.push(KeyModifiers::Control);
                self.key_events.insert(KeyCode::Delete, true);
            },
            10 => {
                self.char_events.push('a');
                self.key_modifiers.push(KeyModifiers::Control);
            },
            _ => {},
        }
        //println!("byte {}: '{}'", byte, byte as char);
    }

    /// Handles a CSI (Control Sequence Introducer) escape sequence.
    /// This method processes the parameters and final character of the escape sequence,
    /// updating the parser state accordingly. It supports mouse events, custom escape codes,
    /// control + arrow keys, and standard escape codes.
    /// The method resets the escape sequence flag and updates the last key press time.
    #[inline(always)]
    fn csi_dispatch(&mut self, params: &vte::Params, _: &[u8], _: bool, c: char) {
        self.in_escape_seq = false;  // resetting the escape sequence
        self.set_press_time();

        let numbers: Vec <u16> = params.iter().map(|p| p[0]).collect();

        // mouse handling
        if c == 'M' || c == 'm' {
            self.handle_mouse_escape_codes(&numbers, c);
            return;
        }

        //for number in &numbers {println!("{}", number);}
        if c == '~' && numbers.len() == 2 && numbers[0] == 3 {  // this section is for custom escape codes
            self.handle_custom_escape_codes(&numbers);
        } else if numbers.len() == 2 && numbers[0] == 1 && numbers[1] == 5 {
            // control + ...
            // 3 = c; 22 = v; 26 = z; 6 = f; 1 = a; 24 = x; 19 = s; 21 = u; r = 18
            // left ^[[1;5D right ^[[1;5C up ^[[1;5A down ^[[1;5B
            // control u and control r and necessary for undo and redo bc/
            // control + key and control + shift + key don't send unique
            // escape codes for some odd reason
            self.handle_control_arrows(&numbers, c);
        } else {  // this checks existing escape codes of 1 parameter/ending code (they don't end with ~)
            self.handle_standard_escape_codes(&numbers, c);
        }
    }
}

