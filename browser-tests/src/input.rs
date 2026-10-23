// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! Keyboard, mouse and touch input, dispatched as trusted browser input through DevTools
//! `Input.*` on the page's own session (the browser routes it to the focused or hit frame,
//! out-of-process iframes included). Key definitions and event fields follow Playwright's US
//! keyboard layout and its Chromium input code.

use serde_json::{Value, json};

use crate::Error;
use crate::connection::lock;
use crate::page::Page;

mod drag;
pub use drag::DragMouse;

/// Keys held down and the mouse position, per page.
#[derive(Debug, Default)]
pub(crate) struct InputState {
    /// Alt = 1, Control = 2, Meta = 4, Shift = 8.
    modifiers: u32,
    mouse_x: f64,
    mouse_y: f64,
    /// Left = 1, right = 2, middle = 4.
    buttons: u32,
}

/// One key of the US layout.
#[derive(Debug, Clone, PartialEq)]
struct Key {
    key: String,
    code: String,
    key_code: u32,
    text: String,
    location: u32,
}

fn named_key(name: &str) -> Option<Key> {
    let (code, key_code, text, location) = match name {
        "Enter" => ("Enter", 13, "\r", 0),
        "Tab" => ("Tab", 9, "", 0),
        "Backspace" => ("Backspace", 8, "", 0),
        "Delete" => ("Delete", 46, "", 0),
        "Escape" => ("Escape", 27, "", 0),
        "Insert" => ("Insert", 45, "", 0),
        "Home" => ("Home", 36, "", 0),
        "End" => ("End", 35, "", 0),
        "PageUp" => ("PageUp", 33, "", 0),
        "PageDown" => ("PageDown", 34, "", 0),
        "ArrowLeft" => ("ArrowLeft", 37, "", 0),
        "ArrowUp" => ("ArrowUp", 38, "", 0),
        "ArrowRight" => ("ArrowRight", 39, "", 0),
        "ArrowDown" => ("ArrowDown", 40, "", 0),
        "Shift" => ("ShiftLeft", 16, "", 1),
        "Control" => ("ControlLeft", 17, "", 1),
        "Alt" => ("AltLeft", 18, "", 1),
        "Meta" => ("MetaLeft", 91, "", 1),
        "CapsLock" => ("CapsLock", 20, "", 0),
        "ContextMenu" => ("ContextMenu", 93, "", 0),
        "Space" | " " => ("Space", 32, " ", 0),
        _ => {
            let number = name.strip_prefix('F')?.parse::<u32>().ok()?;
            if !(1..=12).contains(&number) {
                return None;
            }
            return Some(Key {
                key: name.to_owned(),
                code: name.to_owned(),
                key_code: 111 + number,
                text: String::new(),
                location: 0,
            });
        }
    };
    Some(Key {
        key: if name == "Space" {
            " ".into()
        } else {
            name.to_owned()
        },
        code: code.into(),
        key_code,
        text: text.into(),
        location,
    })
}

/// Printable ASCII: (unshifted, shifted, code, keyCode).
const PRINTABLE: &[(char, char, &str, u32)] = &[
    ('`', '~', "Backquote", 192),
    ('-', '_', "Minus", 189),
    ('=', '+', "Equal", 187),
    ('[', '{', "BracketLeft", 219),
    (']', '}', "BracketRight", 221),
    ('\\', '|', "Backslash", 220),
    (';', ':', "Semicolon", 186),
    ('\'', '"', "Quote", 222),
    (',', '<', "Comma", 188),
    ('.', '>', "Period", 190),
    ('/', '?', "Slash", 191),
    ('1', '!', "Digit1", 49),
    ('2', '@', "Digit2", 50),
    ('3', '#', "Digit3", 51),
    ('4', '$', "Digit4", 52),
    ('5', '%', "Digit5", 53),
    ('6', '^', "Digit6", 54),
    ('7', '&', "Digit7", 55),
    ('8', '*', "Digit8", 56),
    ('9', '(', "Digit9", 57),
    ('0', ')', "Digit0", 48),
];

fn key_for(name: &str) -> Option<Key> {
    if let Some(key) = named_key(name) {
        return Some(key);
    }
    let mut chars = name.chars();
    let character = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let printable = |code: String, key_code: u32| Key {
        key: character.to_string(),
        code,
        key_code,
        text: character.to_string(),
        location: 0,
    };
    if character.is_ascii_alphabetic() {
        let upper = character.to_ascii_uppercase();
        return Some(printable(format!("Key{upper}"), upper as u32));
    }
    PRINTABLE
        .iter()
        .find(|(plain, shifted, _, _)| *plain == character || *shifted == character)
        .map(|(_, _, code, key_code)| printable((*code).to_owned(), *key_code))
}

fn modifier_bit(key: &str) -> u32 {
    match key {
        "Alt" => 1,
        "Control" | "ControlOrMeta" => 2,
        "Meta" => 4,
        "Shift" => 8,
        _ => 0,
    }
}

/// Split `"Control+Shift+a"` into keys; `"+"` alone and a trailing `"++"` mean the plus key.
fn split_combo(combo: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut current = String::new();
    for character in combo.chars() {
        if character == '+' && !current.is_empty() {
            keys.push(std::mem::take(&mut current));
        } else {
            current.push(character);
        }
    }
    keys.push(current);
    keys.into_iter()
        .map(|key| {
            if key == "ControlOrMeta" {
                "Control".to_owned()
            } else {
                key
            }
        })
        .collect()
}

/// The page's keyboard: [`Page::keyboard`].
pub struct Keyboard<'p> {
    page: &'p Page,
}

impl Keyboard<'_> {
    async fn dispatch(&self, params: Value) -> Result<(), Error> {
        self.page.command("Input.dispatchKeyEvent", params).await?;
        Ok(())
    }

    /// Press `key` and keep it down. `key` is a key name (`Enter`, `ArrowDown`, `Shift`, `F5`) or
    /// one printable character.
    pub async fn down(&self, key: &str) -> Result<(), Error> {
        let definition =
            key_for(key).ok_or_else(|| Error::Protocol(format!("unknown key {key:?}")))?;
        let modifiers = {
            let mut input = lock(&self.page.input);
            input.modifiers |= modifier_bit(key);
            input.modifiers
        };
        // With a modifier other than Shift held, a key types nothing.
        let text = if modifiers & !8 != 0 {
            String::new()
        } else {
            definition.text.clone()
        };
        self.dispatch(json!({
            "type": if text.is_empty() { "rawKeyDown" } else { "keyDown" },
            "modifiers": modifiers,
            "windowsVirtualKeyCode": definition.key_code,
            "code": definition.code,
            "key": definition.key,
            "text": text,
            "unmodifiedText": text,
            "autoRepeat": false,
            "location": definition.location,
            "isKeypad": false,
        }))
        .await
    }

    /// Release `key`.
    pub async fn up(&self, key: &str) -> Result<(), Error> {
        let definition =
            key_for(key).ok_or_else(|| Error::Protocol(format!("unknown key {key:?}")))?;
        let modifiers = {
            let mut input = lock(&self.page.input);
            input.modifiers &= !modifier_bit(key);
            input.modifiers
        };
        self.dispatch(json!({
            "type": "keyUp",
            "modifiers": modifiers,
            "key": definition.key,
            "windowsVirtualKeyCode": definition.key_code,
            "code": definition.code,
            "location": definition.location,
        }))
        .await
    }

    /// Press and release a key or a combination such as `"Shift+Tab"` or `"Control+a"`
    /// (Playwright's `keyboard.press`).
    pub async fn press(&self, combo: &str) -> Result<(), Error> {
        let keys = split_combo(combo);
        for key in &keys {
            self.down(key).await?;
        }
        for key in keys.iter().rev() {
            self.up(key).await?;
        }
        Ok(())
    }

    /// Insert `text` as if typed by an input method, without key events (Playwright's
    /// `keyboard.insertText`).
    pub async fn insert_text(&self, text: &str) -> Result<(), Error> {
        self.page
            .command("Input.insertText", json!({"text": text}))
            .await?;
        Ok(())
    }

    /// Type `text` one key at a time; characters outside the US layout are inserted
    /// (Playwright's `keyboard.type`).
    pub async fn type_text(&self, text: &str) -> Result<(), Error> {
        for character in text.chars() {
            let key = character.to_string();
            if character == '\n' {
                self.press("Enter").await?;
            } else if key_for(&key).is_some() {
                self.press(&key).await?;
            } else {
                self.insert_text(&key).await?;
            }
        }
        Ok(())
    }
}

/// Which mouse button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }

    fn bit(self) -> u32 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
        }
    }
}

/// The page's mouse: [`Page::mouse`]. Coordinates are CSS pixels from the top left of the main
/// frame's viewport.
pub struct Mouse<'p> {
    page: &'p Page,
}

impl Mouse<'_> {
    async fn dispatch(&self, params: Value) -> Result<(), Error> {
        self.page
            .command("Input.dispatchMouseEvent", params)
            .await?;
        Ok(())
    }

    /// Move to (`x`, `y`) in `steps` intermediate moves (at least one).
    pub async fn move_to(&self, x: f64, y: f64, steps: u32) -> Result<(), Error> {
        let (from_x, from_y, buttons, modifiers) = {
            let input = lock(&self.page.input);
            (input.mouse_x, input.mouse_y, input.buttons, input.modifiers)
        };
        let steps = steps.max(1);
        for step in 1..=steps {
            let fraction = f64::from(step) / f64::from(steps);
            let (to_x, to_y) = (
                from_x + (x - from_x) * fraction,
                from_y + (y - from_y) * fraction,
            );
            {
                let mut input = lock(&self.page.input);
                input.mouse_x = to_x;
                input.mouse_y = to_y;
            }
            let button = if buttons & 1 != 0 {
                "left"
            } else if buttons & 2 != 0 {
                "right"
            } else if buttons & 4 != 0 {
                "middle"
            } else {
                "none"
            };
            self.dispatch(json!({
                "type": "mouseMoved",
                "button": button,
                "buttons": buttons,
                "x": to_x,
                "y": to_y,
                "modifiers": modifiers,
                "force": if buttons != 0 { 0.5 } else { 0.0 },
            }))
            .await?;
        }
        Ok(())
    }

    /// Press `button` at the current position. `click_count` is 1 for a single click, 2 for the
    /// second press of a double click.
    pub async fn down_with(&self, button: MouseButton, click_count: u32) -> Result<(), Error> {
        let (x, y, buttons, modifiers) = {
            let mut input = lock(&self.page.input);
            input.buttons |= button.bit();
            (input.mouse_x, input.mouse_y, input.buttons, input.modifiers)
        };
        self.dispatch(json!({
            "type": "mousePressed",
            "button": button.name(),
            "buttons": buttons,
            "x": x,
            "y": y,
            "modifiers": modifiers,
            "clickCount": click_count,
            "force": 0.5,
        }))
        .await
    }

    /// Release `button` at the current position.
    pub async fn up_with(&self, button: MouseButton, click_count: u32) -> Result<(), Error> {
        let (x, y, buttons, modifiers) = {
            let mut input = lock(&self.page.input);
            input.buttons &= !button.bit();
            (input.mouse_x, input.mouse_y, input.buttons, input.modifiers)
        };
        self.dispatch(json!({
            "type": "mouseReleased",
            "button": button.name(),
            "buttons": buttons,
            "x": x,
            "y": y,
            "modifiers": modifiers,
            "clickCount": click_count,
            "force": 0.0,
        }))
        .await
    }

    /// Press the left button.
    pub async fn down(&self) -> Result<(), Error> {
        self.down_with(MouseButton::Left, 1).await
    }

    /// Release the left button.
    pub async fn up(&self) -> Result<(), Error> {
        self.up_with(MouseButton::Left, 1).await
    }

    /// Move to (`x`, `y`) and click `click_count` times with `button`.
    pub async fn click_with(
        &self,
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u32,
    ) -> Result<(), Error> {
        self.move_to(x, y, 1).await?;
        for count in 1..=click_count.max(1) {
            self.down_with(button, count).await?;
            self.up_with(button, count).await?;
        }
        Ok(())
    }

    /// Move to (`x`, `y`) and click the left button.
    pub async fn click(&self, x: f64, y: f64) -> Result<(), Error> {
        self.click_with(x, y, MouseButton::Left, 1).await
    }

    /// Move to (`x`, `y`) and double-click.
    pub async fn dblclick(&self, x: f64, y: f64) -> Result<(), Error> {
        self.click_with(x, y, MouseButton::Left, 2).await
    }

    /// Scroll with the wheel at the current position.
    pub async fn wheel(&self, delta_x: f64, delta_y: f64) -> Result<(), Error> {
        let (x, y, modifiers) = {
            let input = lock(&self.page.input);
            (input.mouse_x, input.mouse_y, input.modifiers)
        };
        self.dispatch(json!({
            "type": "mouseWheel",
            "x": x,
            "y": y,
            "modifiers": modifiers,
            "deltaX": delta_x,
            "deltaY": delta_y,
        }))
        .await
    }
}

/// The page's touchscreen: [`Page::touchscreen`]. Call [`Page::set_has_touch`] first, as
/// Playwright requires `hasTouch`.
pub struct Touchscreen<'p> {
    page: &'p Page,
}

impl Touchscreen<'_> {
    /// Touch (`x`, `y`) and lift.
    pub async fn tap(&self, x: f64, y: f64) -> Result<(), Error> {
        let modifiers = lock(&self.page.input).modifiers;
        self.page
            .command(
                "Input.dispatchTouchEvent",
                json!({"type": "touchStart", "touchPoints": [{"x": x, "y": y}], "modifiers": modifiers}),
            )
            .await?;
        self.page
            .command(
                "Input.dispatchTouchEvent",
                json!({"type": "touchEnd", "touchPoints": [], "modifiers": modifiers}),
            )
            .await?;
        Ok(())
    }
}

impl Page {
    pub fn keyboard(&self) -> Keyboard<'_> {
        Keyboard { page: self }
    }

    pub fn mouse(&self) -> Mouse<'_> {
        Mouse { page: self }
    }

    pub fn touchscreen(&self) -> Touchscreen<'_> {
        Touchscreen { page: self }
    }

    /// Emulate a touch screen (Playwright's `hasTouch`), so touch events and `tap` work.
    pub async fn set_has_touch(&self, enabled: bool) -> Result<(), Error> {
        self.command(
            "Emulation.setTouchEmulationEnabled",
            json!({"enabled": enabled, "maxTouchPoints": 1}),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_follow_the_us_layout() {
        let enter = key_for("Enter").unwrap();
        assert_eq!(
            (enter.code.as_str(), enter.key_code, enter.text.as_str()),
            ("Enter", 13, "\r")
        );
        let a = key_for("a").unwrap();
        assert_eq!(
            (a.code.as_str(), a.key_code, a.text.as_str()),
            ("KeyA", 65, "a")
        );
        let bang = key_for("!").unwrap();
        assert_eq!((bang.code.as_str(), bang.key_code), ("Digit1", 49));
        assert_eq!(key_for("F5").unwrap().key_code, 116);
        assert!(key_for("Nope").is_none());
        assert_eq!(split_combo("Shift+Tab"), vec!["Shift", "Tab"]);
        assert_eq!(split_combo("Control++"), vec!["Control", "+"]);
        assert_eq!(split_combo("+"), vec!["+"]);
    }
}
