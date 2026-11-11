//! crossterm's events as tuiframe's, where the view reads them. It goes when
//! tuiframe reads the terminal.

use crossterm::event as ct;
use tuiframe::Size;
use tuiframe::event as tf;

/// `event` as tuiframe names it. A key it has no name for (`Null`) is dropped.
pub(super) fn convert(event: ct::Event) -> Option<tf::Event> {
    Some(match event {
        ct::Event::Key(key) => tf::Event::Key(tf::KeyEvent::new_with_kind(
            key_code(key.code)?,
            modifiers(key.modifiers),
            match key.kind {
                ct::KeyEventKind::Press => tf::KeyEventKind::Press,
                ct::KeyEventKind::Repeat => tf::KeyEventKind::Repeat,
                ct::KeyEventKind::Release => tf::KeyEventKind::Release,
            },
        )),
        ct::Event::Mouse(mouse) => tf::Event::Mouse(tf::MouseEvent::new(
            mouse_kind(mouse.kind),
            mouse.column,
            mouse.row,
            modifiers(mouse.modifiers),
        )),
        ct::Event::Paste(text) => tf::Event::Paste(text),
        ct::Event::FocusGained => tf::Event::FocusGained,
        ct::Event::FocusLost => tf::Event::FocusLost,
        ct::Event::Resize(width, height) => tf::Event::Resize(Size::new(width, height)),
    })
}

fn modifiers(modifiers: ct::KeyModifiers) -> tf::KeyModifiers {
    // (The same bits.)
    tf::KeyModifiers::from_bits_truncate(modifiers.bits())
}

fn key_code(code: ct::KeyCode) -> Option<tf::KeyCode> {
    use ct::KeyCode as C;
    use tf::KeyCode as T;
    Some(match code {
        C::Char(c) => T::Char(c),
        C::Enter => T::Enter,
        C::Tab => T::Tab,
        C::BackTab => T::BackTab,
        C::Backspace => T::Backspace,
        C::Esc => T::Esc,
        C::Left => T::Left,
        C::Right => T::Right,
        C::Up => T::Up,
        C::Down => T::Down,
        C::Home => T::Home,
        C::End => T::End,
        C::PageUp => T::PageUp,
        C::PageDown => T::PageDown,
        C::Insert => T::Insert,
        C::Delete => T::Delete,
        C::F(n) => T::F(n),
        C::Menu => T::Menu,
        C::KeypadBegin => T::KeypadBegin,
        C::CapsLock => T::CapsLock,
        C::ScrollLock => T::ScrollLock,
        C::NumLock => T::NumLock,
        C::PrintScreen => T::PrintScreen,
        C::Pause => T::Pause,
        C::Media(media) => T::Media(media_key(media)),
        C::Modifier(key) => T::Modifier(modifier_key(key)),
        C::Null => return None,
    })
}

fn media_key(key: ct::MediaKeyCode) -> tf::MediaKey {
    use ct::MediaKeyCode as C;
    use tf::MediaKey as T;
    match key {
        C::Play => T::Play,
        C::Pause => T::Pause,
        C::PlayPause => T::PlayPause,
        C::Reverse => T::Reverse,
        C::Stop => T::Stop,
        C::FastForward => T::FastForward,
        C::Rewind => T::Rewind,
        C::TrackNext => T::TrackNext,
        C::TrackPrevious => T::TrackPrevious,
        C::Record => T::Record,
        C::LowerVolume => T::LowerVolume,
        C::RaiseVolume => T::RaiseVolume,
        C::MuteVolume => T::MuteVolume,
    }
}

fn modifier_key(key: ct::ModifierKeyCode) -> tf::ModifierKey {
    use ct::ModifierKeyCode as C;
    use tf::ModifierKey as T;
    match key {
        C::LeftShift => T::LeftShift,
        C::LeftControl => T::LeftControl,
        C::LeftAlt => T::LeftAlt,
        C::LeftSuper => T::LeftSuper,
        C::LeftHyper => T::LeftHyper,
        C::LeftMeta => T::LeftMeta,
        C::RightShift => T::RightShift,
        C::RightControl => T::RightControl,
        C::RightAlt => T::RightAlt,
        C::RightSuper => T::RightSuper,
        C::RightHyper => T::RightHyper,
        C::RightMeta => T::RightMeta,
        C::IsoLevel3Shift => T::IsoLevel3Shift,
        C::IsoLevel5Shift => T::IsoLevel5Shift,
    }
}

fn mouse_kind(kind: ct::MouseEventKind) -> tf::MouseEventKind {
    use ct::MouseEventKind as C;
    use tf::MouseEventKind as T;
    match kind {
        C::Down(button) => T::Down(mouse_button(button)),
        C::Up(button) => T::Up(mouse_button(button)),
        C::Drag(button) => T::Drag(mouse_button(button)),
        C::Moved => T::Moved,
        C::ScrollUp => T::ScrollUp,
        C::ScrollDown => T::ScrollDown,
        C::ScrollLeft => T::ScrollLeft,
        C::ScrollRight => T::ScrollRight,
    }
}

fn mouse_button(button: ct::MouseButton) -> tf::MouseButton {
    match button {
        ct::MouseButton::Left => tf::MouseButton::Left,
        ct::MouseButton::Right => tf::MouseButton::Right,
        ct::MouseButton::Middle => tf::MouseButton::Middle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: ct::KeyCode, modifiers: ct::KeyModifiers, kind: ct::KeyEventKind) -> ct::Event {
        ct::Event::Key(ct::KeyEvent::new_with_kind(code, modifiers, kind))
    }

    /// Every key that Arko matches, with each modifier it reads and each kind
    /// of event, comes out as the same key.
    #[test]
    fn the_keys_arko_handles_keep_their_code_modifiers_and_kind() {
        let codes = [
            (ct::KeyCode::Char('a'), tf::KeyCode::Char('a')),
            (ct::KeyCode::Char('Ž'), tf::KeyCode::Char('Ž')),
            (ct::KeyCode::Enter, tf::KeyCode::Enter),
            (ct::KeyCode::Down, tf::KeyCode::Down),
            (ct::KeyCode::Up, tf::KeyCode::Up),
            (ct::KeyCode::Right, tf::KeyCode::Right),
            (ct::KeyCode::Left, tf::KeyCode::Left),
            (ct::KeyCode::Tab, tf::KeyCode::Tab),
            (ct::KeyCode::Esc, tf::KeyCode::Esc),
            (ct::KeyCode::Home, tf::KeyCode::Home),
            (ct::KeyCode::End, tf::KeyCode::End),
            (ct::KeyCode::Backspace, tf::KeyCode::Backspace),
            (ct::KeyCode::PageDown, tf::KeyCode::PageDown),
            (ct::KeyCode::PageUp, tf::KeyCode::PageUp),
            (ct::KeyCode::BackTab, tf::KeyCode::BackTab),
            (ct::KeyCode::Delete, tf::KeyCode::Delete),
        ];
        let modifiers = [
            (ct::KeyModifiers::NONE, tf::KeyModifiers::NONE),
            (ct::KeyModifiers::CONTROL, tf::KeyModifiers::CONTROL),
            (ct::KeyModifiers::ALT, tf::KeyModifiers::ALT),
            (ct::KeyModifiers::SHIFT, tf::KeyModifiers::SHIFT),
            (
                ct::KeyModifiers::CONTROL | ct::KeyModifiers::ALT | ct::KeyModifiers::SHIFT,
                tf::KeyModifiers::CONTROL | tf::KeyModifiers::ALT | tf::KeyModifiers::SHIFT,
            ),
            (
                ct::KeyModifiers::SUPER | ct::KeyModifiers::HYPER | ct::KeyModifiers::META,
                tf::KeyModifiers::SUPER | tf::KeyModifiers::HYPER | tf::KeyModifiers::META,
            ),
        ];
        let kinds = [
            (ct::KeyEventKind::Press, tf::KeyEventKind::Press),
            (ct::KeyEventKind::Repeat, tf::KeyEventKind::Repeat),
            (ct::KeyEventKind::Release, tf::KeyEventKind::Release),
        ];
        for (code, expected_code) in codes {
            for (held, expected_held) in modifiers {
                for (kind, expected_kind) in kinds {
                    assert_eq!(
                        convert(key(code, held, kind)),
                        Some(tf::Event::Key(tf::KeyEvent::new_with_kind(
                            expected_code,
                            expected_held,
                            expected_kind
                        ))),
                        "{code:?} {held:?} {kind:?}"
                    );
                }
            }
        }
    }

    /// The keys that only some terminals send come across too, and `Null`,
    /// which tuiframe has no key for, is dropped.
    #[test]
    fn the_other_keys_come_across_and_null_is_dropped() {
        let pairs = [
            (ct::KeyCode::Insert, tf::KeyCode::Insert),
            (ct::KeyCode::F(5), tf::KeyCode::F(5)),
            (ct::KeyCode::Menu, tf::KeyCode::Menu),
            (ct::KeyCode::KeypadBegin, tf::KeyCode::KeypadBegin),
            (ct::KeyCode::CapsLock, tf::KeyCode::CapsLock),
            (ct::KeyCode::ScrollLock, tf::KeyCode::ScrollLock),
            (ct::KeyCode::NumLock, tf::KeyCode::NumLock),
            (ct::KeyCode::PrintScreen, tf::KeyCode::PrintScreen),
            (ct::KeyCode::Pause, tf::KeyCode::Pause),
            (
                ct::KeyCode::Media(ct::MediaKeyCode::PlayPause),
                tf::KeyCode::Media(tf::MediaKey::PlayPause),
            ),
            (
                ct::KeyCode::Modifier(ct::ModifierKeyCode::RightAlt),
                tf::KeyCode::Modifier(tf::ModifierKey::RightAlt),
            ),
        ];
        for (code, expected) in pairs {
            let sent = key(code, ct::KeyModifiers::NONE, ct::KeyEventKind::Press);
            assert_eq!(
                convert(sent),
                Some(tf::Event::Key(tf::KeyEvent::new(
                    expected,
                    tf::KeyModifiers::NONE
                )))
            );
        }
        let null = key(
            ct::KeyCode::Null,
            ct::KeyModifiers::NONE,
            ct::KeyEventKind::Press,
        );
        assert_eq!(convert(null), None);
    }

    #[test]
    fn mouse_events_keep_their_kind_place_and_modifiers() {
        let kinds = [
            (
                ct::MouseEventKind::Down(ct::MouseButton::Left),
                tf::MouseEventKind::Down(tf::MouseButton::Left),
            ),
            (
                ct::MouseEventKind::Drag(ct::MouseButton::Left),
                tf::MouseEventKind::Drag(tf::MouseButton::Left),
            ),
            (
                ct::MouseEventKind::Up(ct::MouseButton::Left),
                tf::MouseEventKind::Up(tf::MouseButton::Left),
            ),
            (
                ct::MouseEventKind::Down(ct::MouseButton::Right),
                tf::MouseEventKind::Down(tf::MouseButton::Right),
            ),
            (
                ct::MouseEventKind::Up(ct::MouseButton::Middle),
                tf::MouseEventKind::Up(tf::MouseButton::Middle),
            ),
            (ct::MouseEventKind::ScrollUp, tf::MouseEventKind::ScrollUp),
            (
                ct::MouseEventKind::ScrollDown,
                tf::MouseEventKind::ScrollDown,
            ),
            (
                ct::MouseEventKind::ScrollLeft,
                tf::MouseEventKind::ScrollLeft,
            ),
            (
                ct::MouseEventKind::ScrollRight,
                tf::MouseEventKind::ScrollRight,
            ),
            (ct::MouseEventKind::Moved, tf::MouseEventKind::Moved),
        ];
        for (kind, expected) in kinds {
            let sent = ct::Event::Mouse(ct::MouseEvent {
                kind,
                column: 12,
                row: 7,
                modifiers: ct::KeyModifiers::CONTROL,
            });
            assert_eq!(
                convert(sent),
                Some(tf::Event::Mouse(tf::MouseEvent::new(
                    expected,
                    12,
                    7,
                    tf::KeyModifiers::CONTROL
                ))),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn paste_focus_and_resize_come_across() {
        assert_eq!(
            convert(ct::Event::Paste("one\ntwo".into())),
            Some(tf::Event::Paste("one\ntwo".into()))
        );
        assert_eq!(
            convert(ct::Event::FocusGained),
            Some(tf::Event::FocusGained)
        );
        assert_eq!(convert(ct::Event::FocusLost), Some(tf::Event::FocusLost));
        assert_eq!(
            convert(ct::Event::Resize(80, 24)),
            Some(tf::Event::Resize(Size::new(80, 24)))
        );
    }
}
