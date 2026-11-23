//! Translates stream input and gestures without depending on the frontend.

use std::time::Instant;

use hestia_core::stream::InputEvent;
use hestia_core::stream::input::{Modifiers, MouseButton, PenTool, TouchPhase, vk};

use crate::input::{PointerPhase, PointerTool, RawPointer, TouchRouter};
use crate::keymap::{self, Mapped};
use crate::touch::{self, Trackpad};
use crate::typing;
use crate::virtual_pad::{PadState, VirtualPad};

const TOUCH_SCROLL_SPEED: f32 = 2.0;
const TRACKPAD_SPEED: f32 = 1.4;
const MAX_ZOOM: f32 = 4.0;

/// Geometry of the video relative to the stream view, in logical pixels.
pub(crate) struct Picture {
    pub(crate) centre: (f32, f32),
    pub(crate) size: (f32, f32),
}

struct PinchStart {
    distance: f32,
    mid: (f32, f32),
    zoom: f32,
    pan: (f32, f32),
    centre: (f32, f32),
    size: (f32, f32),
}

struct Zoom {
    on: bool,
    factor: f32,
    pan: (f32, f32),
    pinch: Option<PinchStart>,
}

impl Default for Zoom {
    fn default() -> Self {
        Self {
            on: false,
            factor: 1.0,
            pan: (0.0, 0.0),
            pinch: None,
        }
    }
}

pub(crate) enum GestureAction {
    Pointer,
    Consumed,
    Zoom(f32, (f32, f32)),
    ToggleKeyboard,
}

/// What a platform's soft keyboard sent outside ordinary key events.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) enum ImeEvent {
    Text(String),
    DeleteBefore(usize),
}

/// Keys and mouse buttons the host holds down because of us. When the
/// stream loses the keyboard (another window, the menu) their releases go
/// elsewhere, so they are all let go at once instead of repeating or
/// staying pressed on the host.
#[derive(Debug, Default)]
pub(crate) struct Held {
    keys: Vec<u16>,
    buttons: Vec<MouseButton>,
}

impl Held {
    /// Notes what `event` presses or releases.
    pub(crate) fn track(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::Key { key, pressed, .. } => {
                self.keys.retain(|&k| k != key);
                if pressed {
                    self.keys.push(key);
                }
            }
            InputEvent::MouseButton { button, pressed } => {
                self.buttons.retain(|&b| b != button);
                if pressed {
                    self.buttons.push(button);
                }
            }
            _ => {}
        }
    }

    /// The modifiers held right now.
    pub(crate) fn modifiers(&self) -> Modifiers {
        let held = |keys: &[u16]| self.keys.iter().any(|k| keys.contains(k));
        Modifiers {
            shift: held(&[vk::SHIFT, vk::LSHIFT, vk::RSHIFT]),
            ctrl: held(&[vk::CONTROL, vk::LCONTROL, vk::RCONTROL]),
            alt: held(&[vk::MENU, vk::LMENU, vk::RMENU]),
            meta: held(&[vk::LWIN, vk::RWIN]),
        }
    }

    /// Releases for everything held: keys, then modifiers, then buttons.
    pub(crate) fn release_all(&mut self) -> Vec<InputEvent> {
        let mut keys = std::mem::take(&mut self.keys);
        keys.sort_by_key(|&k| vk::is_modifier(k));
        let buttons = std::mem::take(&mut self.buttons);
        keys.into_iter()
            .map(|key| InputEvent::Key {
                key,
                pressed: false,
                modifiers: Modifiers::default(),
            })
            .chain(
                buttons
                    .into_iter()
                    .map(|button| InputEvent::MouseButton { button, pressed: false }),
            )
            .collect()
    }
}

/// Where the video is in the stream view, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct VideoRect {
    pub(crate) origin: (f32, f32),
    pub(crate) size: (f32, f32),
}

impl VideoRect {
    /// `point` (in the view) relative to the video.
    pub(crate) fn local(&self, (x, y): (f32, f32)) -> (f32, f32) {
        (x - self.origin.0, y - self.origin.1)
    }

    pub(crate) fn picture(&self) -> Picture {
        Picture {
            centre: (self.origin.0 + self.size.0 / 2.0, self.origin.1 + self.size.1 / 2.0),
            size: self.size,
        }
    }
}

pub(crate) struct StreamInput {
    width: u32,
    trackpad: Trackpad,
    touch_id: u64,
    fingers: touch::FingerTaps,
    pad: VirtualPad,
    pad_visible: bool,
    view: (f32, f32),
    zoom: Zoom,
    /// Logical pixels to physical ones (raw pointers come in physical).
    scale: f32,
    video: VideoRect,
    /// The platform hands over every finger itself ([`RawPointer`]s); the
    /// toolkit's touches only say which touch is the stream's.
    pub(crate) raw_touch: bool,
    pub(crate) router: TouchRouter,
    /// The stream view has the keyboard (window focused, menu closed).
    pub(crate) focused: bool,
    /// Pointer capture asked for: the host gets relative motion.
    pub(crate) capture: bool,
    motion_remainder: (f64, f64),
    /// The host takes pen and touch events.
    pub(crate) pen_and_touch: bool,
}

impl StreamInput {
    pub(crate) fn new(width: u32, view: (f32, f32), pad_visible: bool) -> Self {
        let mut pad = VirtualPad::default();
        pad.resize(view.0, view.1);
        Self {
            width,
            trackpad: Trackpad::new(),
            touch_id: 0,
            fingers: touch::FingerTaps::default(),
            pad,
            pad_visible,
            view,
            zoom: Zoom::default(),
            scale: 1.0,
            video: VideoRect {
                origin: (0.0, 0.0),
                size: view,
            },
            raw_touch: false,
            router: TouchRouter::default(),
            focused: true,
            capture: false,
            motion_remainder: (0.0, 0.0),
            pen_and_touch: false,
        }
    }

    pub(crate) fn view_size(&self) -> (f32, f32) {
        self.view
    }

    pub(crate) fn scale(&self) -> f32 {
        self.scale
    }

    pub(crate) fn resize(&mut self, width: f32, height: f32, scale: f32) -> bool {
        if scale.is_finite() && scale > 0.0 {
            self.scale = scale;
        }
        if self.view == (width, height) {
            return false;
        }
        self.view = (width, height);
        self.pad.resize(width, height);
        self.pad_visible
    }

    /// The toolkit's latest word on where the video is.
    pub(crate) fn set_video(&mut self, video: VideoRect) {
        self.video = video;
    }

    pub(crate) fn video(&self) -> VideoRect {
        self.video
    }

    /// A raw pointer's position in the stream view (logical pixels).
    pub(crate) fn view_point(&self, pointer: &RawPointer) -> (f32, f32) {
        (pointer.x / self.scale, pointer.y / self.scale)
    }

    /// Pointer capture is on and the stream has the input: relative motion.
    pub(crate) fn captured(&self) -> bool {
        self.capture && self.focused
    }

    /// Relative mouse motion while captured, with sub-pixel motion carried over.
    pub(crate) fn relative_motion(&mut self, dx: f64, dy: f64) -> Option<InputEvent> {
        let (fx, fy) = (dx + self.motion_remainder.0, dy + self.motion_remainder.1);
        let (ix, iy) = (fx.trunc(), fy.trunc());
        self.motion_remainder = (fx - ix, fy - iy);
        let clamp = |v: f64| v.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        let (dx, dy) = (clamp(ix), clamp(iy));
        (dx != 0 || dy != 0).then_some(InputEvent::MouseMove { dx, dy })
    }

    pub(crate) fn toggle_pad(&mut self) -> bool {
        self.pad_visible = !self.pad_visible;
        self.pad.resize(self.view.0, self.view.1);
        self.pad_visible
    }

    pub(crate) fn pad(&self) -> &VirtualPad {
        &self.pad
    }

    #[cfg(target_os = "android")]
    pub(crate) fn reset_pad(&mut self) {
        self.pad.resize(self.view.0, self.view.1);
        self.pad.take_click();
    }

    pub(crate) fn visible_pad_state(&self) -> Option<PadState> {
        self.pad_visible.then(|| self.pad.state())
    }

    /// Returns the on-screen controller state and its pending stick click.
    pub(crate) fn pad_event(
        &mut self,
        kind: i32,
        finger: i32,
        (x, y): (f32, f32),
        now: Instant,
    ) -> Option<(PadState, u32)> {
        if !self.pad_visible {
            return None;
        }
        match kind {
            0 if self.pad.down(finger, x, y, now) => {}
            2 if self.pad.owns(finger) => self.pad.moved(finger, x, y),
            1 | 3 if self.pad.owns(finger) => self.pad.up(finger, now),
            _ => return None,
        }
        Some((self.pad.state(), self.pad.take_click()))
    }

    pub(crate) fn toggle_zoom(&mut self) -> bool {
        self.zoom = Zoom {
            on: !self.zoom.on,
            ..Zoom::default()
        };
        self.zoom.on
    }

    /// The trackpad and direct pointer only follow the first finger. A second
    /// finger cancels a pending click or drag and starts a gesture.
    pub(crate) fn multi_finger(
        &mut self,
        kind: i32,
        finger: i32,
        (x, y): (f32, f32),
        picture: Picture,
        now: Instant,
        mut emit: impl FnMut(InputEvent),
    ) -> GestureAction {
        let consumed = match kind {
            0 => {
                let was_multi = self.fingers.multi();
                self.fingers.down(finger, x, y, now);
                if self.fingers.multi() && !was_multi {
                    self.touch_id += 1;
                    trackpad_events(self.trackpad.cancel()).for_each(&mut emit);
                }
                self.fingers.multi()
            }
            2 => {
                let (dx, dy) = self.fingers.moved(finger, x, y);
                if self.zoom.on && self.fingers.count() == 2 {
                    return match self.pinch(picture) {
                        Some((factor, pan)) => GestureAction::Zoom(factor, pan),
                        None => GestureAction::Consumed,
                    };
                }
                // The first finger leads the two-finger scroll gesture.
                if self.fingers.count() == 2 && self.fingers.primary() == Some(finger) {
                    let amount = |v: f32| (v * TOUCH_SCROLL_SPEED).round().clamp(-32767.0, 32767.0) as i16;
                    let (horizontal, vertical) = (amount(-dx), amount(dy));
                    if horizontal != 0 || vertical != 0 {
                        emit(InputEvent::Scroll { vertical, horizontal });
                    }
                }
                self.fingers.multi() || self.fingers.primary() != Some(finger)
            }
            1 | 3 => {
                self.zoom.pinch = None;
                let secondary = self.fingers.multi() || self.fingers.primary() != Some(finger);
                let tap = if kind == 1 {
                    self.fingers.up(finger, now)
                } else {
                    self.fingers.reset();
                    None
                };
                match tap {
                    Some(2) => {
                        for pressed in [true, false] {
                            emit(InputEvent::MouseButton {
                                button: MouseButton::Right,
                                pressed,
                            });
                        }
                    }
                    Some(3..) => return GestureAction::ToggleKeyboard,
                    _ => {}
                }
                secondary || tap.is_some()
            }
            _ => false,
        };
        if consumed {
            GestureAction::Consumed
        } else {
            GestureAction::Pointer
        }
    }

    /// Zooms about the midpoint between two fingers and pans with it.
    fn pinch(&mut self, picture: Picture) -> Option<(f32, (f32, f32))> {
        let ((ax, ay), (bx, by)) = self.fingers.pair()?;
        let distance = (bx - ax).hypot(by - ay).max(1.0);
        let mid = ((ax + bx) / 2.0, (ay + by) / 2.0);
        let z = &mut self.zoom;
        let start = z.pinch.get_or_insert(PinchStart {
            distance,
            mid,
            zoom: z.factor,
            pan: z.pan,
            centre: picture.centre,
            size: (picture.size.0 / z.factor, picture.size.1 / z.factor),
        });
        let factor = (start.zoom * distance / start.distance).clamp(1.0, MAX_ZOOM);
        // The picture point under the fingers stays under them.
        let anchor = (
            (start.mid.0 - start.centre.0) / start.zoom,
            (start.mid.1 - start.centre.1) / start.zoom,
        );
        let base = (start.centre.0 - start.pan.0, start.centre.1 - start.pan.1);
        let limit = |size: f32| size * (factor - 1.0) / 2.0;
        let pan = (
            (mid.0 - anchor.0 * factor - base.0).clamp(-limit(start.size.0), limit(start.size.0)),
            (mid.1 - anchor.1 * factor - base.1).clamp(-limit(start.size.1), limit(start.size.1)),
        );
        if (factor, pan) == (z.factor, z.pan) {
            return None;
        }
        (z.factor, z.pan) = (factor, pan);
        Some((factor, pan))
    }

    /// Applies a trackpad action and returns a token if a long press should
    /// be scheduled by the frontend.
    pub(crate) fn trackpad_event(
        &mut self,
        kind: i32,
        (x, y): (f32, f32),
        video_width: f32,
        now: Instant,
        emit: impl FnMut(InputEvent),
    ) -> Option<u64> {
        let outputs = match kind {
            0 => {
                self.touch_id += 1;
                self.trackpad.down(x, y, now)
            }
            1 => self.trackpad.up(now),
            2 => {
                let scale = TRACKPAD_SPEED * self.width as f32 / video_width.max(1.0);
                self.trackpad.moved(x, y, scale)
            }
            _ => self.trackpad.cancel(),
        };
        trackpad_events(outputs).for_each(emit);
        (kind == 0).then_some(self.touch_id)
    }

    pub(crate) fn long_press(&mut self, touch_id: u64, now: Instant, emit: impl FnMut(InputEvent)) {
        if self.touch_id == touch_id {
            trackpad_events(self.trackpad.long_press(now)).for_each(emit);
        }
    }

    pub(crate) fn reset_touch(&mut self, emit: impl FnMut(InputEvent)) {
        trackpad_events(self.trackpad.cancel()).for_each(emit);
        self.fingers.reset();
    }
}

fn trackpad_events(outputs: Vec<touch::Output>) -> impl Iterator<Item = InputEvent> {
    outputs.into_iter().map(|o| match o {
        touch::Output::Move { dx, dy } => InputEvent::MouseMove { dx, dy },
        touch::Output::Button { button, pressed } => InputEvent::MouseButton { button, pressed },
    })
}

pub(crate) fn key(text: &str, pressed: bool, modifiers: Modifiers) -> Option<InputEvent> {
    match keymap::map(text) {
        Some(Mapped::Key(key)) => Some(InputEvent::Key {
            key,
            pressed,
            modifiers,
        }),
        Some(Mapped::Text(text)) if pressed => Some(InputEvent::Text(text)),
        _ => None,
    }
}

/// The frontend coordinates are relative to the video rather than the screen.
pub(crate) fn absolute_pointer(
    kind: i32,
    button: i32,
    (x, y): (f32, f32),
    (width, height): (f32, f32),
    touch: bool,
) -> impl Iterator<Item = InputEvent> {
    if width < 1.0 || height < 1.0 {
        return [None, None].into_iter().flatten();
    }
    let clamp = |v: f32, max: f32| v.clamp(0.0, max - 1.0).round() as i16;
    let position = InputEvent::MousePosition {
        x: clamp(x, width),
        y: clamp(y, height),
        width: width.round().min(i16::MAX as f32) as i16,
        height: height.round().min(i16::MAX as f32) as i16,
    };
    let button = match button {
        _ if touch => Some(MouseButton::Left),
        button => mouse_button(button),
    };
    let click = match (button, kind) {
        (Some(button), 0 | 1) => Some(InputEvent::MouseButton {
            button,
            pressed: kind == 0,
        }),
        _ => None,
    };
    [Some(position), click].into_iter().flatten()
}

/// A mouse button by the number the frontend gives it: 1 left, 2 middle,
/// 3 right, 4 back, 5 forward.
pub(crate) fn mouse_button(button: i32) -> Option<MouseButton> {
    Some(match button {
        1 => MouseButton::Left,
        2 => MouseButton::Middle,
        3 => MouseButton::Right,
        4 => MouseButton::X1,
        5 => MouseButton::X2,
        _ => return None,
    })
}

/// A captured mouse's button (no position: the host's cursor is where the
/// relative motion put it).
pub(crate) fn captured_button(kind: i32, button: i32) -> Option<InputEvent> {
    match (mouse_button(button), kind) {
        (Some(button), 0 | 1) => Some(InputEvent::MouseButton {
            button,
            pressed: kind == 0,
        }),
        _ => None,
    }
}

pub(crate) fn native_touch(
    kind: i32,
    finger: i32,
    (x, y): (f32, f32),
    (width, height): (f32, f32),
    pressure: Option<f32>,
) -> Option<InputEvent> {
    if width < 1.0 || height < 1.0 {
        return None;
    }
    let phase = match kind {
        0 => TouchPhase::Down,
        1 => TouchPhase::Up,
        2 => TouchPhase::Move,
        _ => TouchPhase::Cancel,
    };
    let pressure = match kind {
        1 => 0.0,
        // Some screens report 0 for a light touch: a touch is never weightless.
        _ => pressure.map_or(1.0, |p| p.clamp(0.01, 1.0)),
    };
    Some(InputEvent::Touch {
        pointer_id: finger as u32,
        phase,
        x: (x / width).clamp(0.0, 1.0),
        y: (y / height).clamp(0.0, 1.0),
        pressure,
    })
}

/// A pen on (or over) the video. `(x, y)` is relative to the video.
pub(crate) fn pen(pointer: &RawPointer, (x, y): (f32, f32), (width, height): (f32, f32)) -> Option<InputEvent> {
    if width < 1.0 || height < 1.0 || !pointer.tool.is_pen() {
        return None;
    }
    let phase = match pointer.phase {
        PointerPhase::Down => TouchPhase::Down,
        PointerPhase::Move => TouchPhase::Move,
        PointerPhase::Up => TouchPhase::Up,
        PointerPhase::Cancel => TouchPhase::Cancel,
        PointerPhase::Hover => TouchPhase::Hover,
        PointerPhase::HoverLeave => TouchPhase::HoverLeave,
    };
    let pressure = match pointer.phase {
        PointerPhase::Down | PointerPhase::Move => pointer.pressure.map_or(1.0, |p| p.clamp(0.01, 1.0)),
        PointerPhase::Hover => pointer.distance.unwrap_or(0.0).clamp(0.0, 1.0),
        _ => 0.0,
    };
    Some(InputEvent::Pen {
        tool: if pointer.tool == PointerTool::Eraser {
            PenTool::Eraser
        } else {
            PenTool::Pen
        },
        phase,
        x: (x / width).clamp(0.0, 1.0),
        y: (y / height).clamp(0.0, 1.0),
        pressure,
        tilt: pointer.tilt.filter(|t| t.is_finite()).map(|t| t.clamp(0.0, 90.0)),
        rotation: pointer.rotation.filter(|r| r.is_finite()).map(|r| r.rem_euclid(360.0)),
        buttons: pointer.buttons,
    })
}

pub(crate) fn ime(event: ImeEvent, hermes: bool, mut emit: impl FnMut(InputEvent)) {
    match event {
        ImeEvent::Text(text) if hermes && text.bytes().all(|byte| (b' '..=b'~').contains(&byte)) => {
            // Hermes' Linux keyboard can inject UTF-8 via Ctrl+Shift+U and
            // Return. A greeter can see the Return as a submit. Use the same
            // US-layout key mapping as the soft keyboard's ordinary keys.
            for (key, shift) in text.chars().filter_map(vk::from_char) {
                for pressed in [true, false] {
                    emit(InputEvent::Key {
                        key,
                        pressed,
                        modifiers: Modifiers {
                            shift,
                            ..Modifiers::default()
                        },
                    });
                }
            }
        }
        ImeEvent::Text(text) => typing::edit_events("", &text).into_iter().for_each(emit),
        ImeEvent::DeleteBefore(count) => {
            for _ in 0..count.min(256) {
                typing::special_key_events("backspace").into_iter().for_each(&mut emit);
            }
        }
    }
}

/// About 60 logical pixels per wheel notch on the frontend, 120 units on the host.
pub(crate) fn scroll(dx: f32, dy: f32) -> Option<InputEvent> {
    let amount = |v: f32| (v * 2.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    let (horizontal, vertical) = (amount(dx), amount(dy));
    (horizontal != 0 || vertical != 0).then_some(InputEvent::Scroll { vertical, horizontal })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermes_ime_commits_ascii_as_key_taps_with_implied_shift() {
        let mut keys = Vec::new();
        ime(ImeEvent::Text("aA! ".into()), true, |event| match event {
            InputEvent::Key {
                key,
                pressed,
                modifiers,
            } => keys.push((key, pressed, modifiers.shift)),
            _ => panic!("expected keyboard events"),
        });
        assert_eq!(
            keys,
            [
                (b'A' as u16, true, false),
                (b'A' as u16, false, false),
                (b'A' as u16, true, true),
                (b'A' as u16, false, true),
                (b'1' as u16, true, true),
                (b'1' as u16, false, true),
                (vk::SPACE, true, false),
                (vk::SPACE, false, false),
            ]
        );
    }

    #[test]
    fn ime_preserves_utf8_for_other_hosts_and_non_ascii_text() {
        for (text, hermes) in [("A!", false), ("café", true), ("a\nb", true)] {
            let mut events = Vec::new();
            ime(ImeEvent::Text(text.into()), hermes, |event| events.push(event));
            assert!(matches!(&events[..], [InputEvent::Text(actual)] if actual == text));
        }
    }

    #[test]
    fn direct_touch_sends_position_before_click_and_clamps_to_video() {
        let mut events = absolute_pointer(0, 0, (-5.0, 150.0), (200.0, 100.0), true);
        assert!(matches!(
            events.next(),
            Some(InputEvent::MousePosition {
                x: 0,
                y: 99,
                width: 200,
                height: 100
            })
        ));
        assert!(matches!(
            events.next(),
            Some(InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true
            })
        ));
        assert!(events.next().is_none());
        assert!(absolute_pointer(0, 1, (0.0, 0.0), (0.0, 0.0), false).next().is_none());
    }

    #[test]
    fn a_second_finger_invalidates_the_pending_long_press() {
        let mut input = StreamInput::new(1920, (200.0, 100.0), false);
        let now = Instant::now();
        let picture = || Picture {
            centre: (100.0, 50.0),
            size: (200.0, 100.0),
        };
        input.multi_finger(0, 1, (20.0, 20.0), picture(), now, |_| {});
        let token = input.trackpad_event(0, (20.0, 20.0), 200.0, now, |_| {}).unwrap();

        assert!(matches!(
            input.multi_finger(0, 2, (50.0, 20.0), picture(), now, |_| {}),
            GestureAction::Consumed
        ));
        input.long_press(token, now + touch::LONG_PRESS, |_| panic!("stale click"));
    }

    fn key(key: u16, pressed: bool) -> InputEvent {
        InputEvent::Key {
            key,
            pressed,
            modifiers: Modifiers::default(),
        }
    }

    #[test]
    fn held_keys_and_buttons_are_released_together() {
        let mut held = Held::default();
        for e in [
            key(vk::LSHIFT, true),
            key(b'W' as u16, true),
            key(b'A' as u16, true),
            key(b'A' as u16, false),
            InputEvent::MouseButton {
                button: MouseButton::Left,
                pressed: true,
            },
            InputEvent::MouseMove { dx: 1, dy: 1 },
        ] {
            held.track(&e);
        }
        assert!(held.modifiers().shift);
        assert!(!held.modifiers().ctrl);
        let released = held.release_all();
        assert_eq!(
            released,
            vec![
                key(b'W' as u16, false),
                key(vk::LSHIFT, false),
                InputEvent::MouseButton {
                    button: MouseButton::Left,
                    pressed: false
                },
            ]
        );
        assert!(held.release_all().is_empty(), "nothing twice");
        assert_eq!(held.modifiers(), Modifiers::default());
    }

    #[test]
    fn right_alt_counts_as_alt() {
        let mut held = Held::default();
        held.track(&key(vk::RMENU, true));
        held.track(&key(vk::RCONTROL, true));
        let m = held.modifiers();
        assert!(m.alt && m.ctrl && !m.shift && !m.meta);
    }

    #[test]
    fn relative_motion_keeps_fractions() {
        let mut input = StreamInput::new(1920, (200.0, 100.0), false);
        assert_eq!(input.relative_motion(0.6, -0.4), None);
        assert_eq!(
            input.relative_motion(0.6, -0.8),
            Some(InputEvent::MouseMove { dx: 1, dy: -1 })
        );
        assert_eq!(
            input.relative_motion(1e9, 0.0),
            Some(InputEvent::MouseMove { dx: i16::MAX, dy: 0 })
        );
    }

    #[test]
    fn captured_flags_need_focus() {
        let mut input = StreamInput::new(1920, (200.0, 100.0), false);
        input.capture = true;
        assert!(input.captured());
        input.focused = false;
        assert!(!input.captured());
    }

    #[test]
    fn raw_pointers_are_scaled_to_the_view() {
        let mut input = StreamInput::new(1920, (200.0, 100.0), false);
        input.resize(200.0, 100.0, 2.0);
        let p = RawPointer::finger(0, PointerPhase::Down, 50.0, 30.0);
        assert_eq!(input.view_point(&p), (25.0, 15.0));
        input.resize(200.0, 100.0, f32::NAN);
        assert_eq!(input.scale(), 2.0, "a bad scale is ignored");
        let video = VideoRect {
            origin: (20.0, 10.0),
            size: (160.0, 90.0),
        };
        assert_eq!(video.local((25.0, 15.0)), (5.0, 5.0));
        assert_eq!(video.picture().centre, (100.0, 55.0));
    }

    #[test]
    fn pens_carry_pressure_tilt_and_buttons() {
        let mut p = RawPointer {
            tool: PointerTool::Pen,
            pressure: Some(0.4),
            tilt: Some(120.0),
            rotation: Some(-30.0),
            buttons: hestia_core::stream::input::pen_buttons::PRIMARY,
            ..RawPointer::finger(3, PointerPhase::Move, 0.0, 0.0)
        };
        let event = pen(&p, (50.0, 25.0), (100.0, 100.0)).unwrap();
        assert_eq!(
            event,
            InputEvent::Pen {
                tool: PenTool::Pen,
                phase: TouchPhase::Move,
                x: 0.5,
                y: 0.25,
                pressure: 0.4,
                tilt: Some(90.0),
                rotation: Some(330.0),
                buttons: 1,
            }
        );
        p.phase = PointerPhase::Hover;
        p.distance = Some(0.3);
        p.tool = PointerTool::Eraser;
        assert!(matches!(
            pen(&p, (0.0, 0.0), (100.0, 100.0)),
            Some(InputEvent::Pen {
                tool: PenTool::Eraser,
                phase: TouchPhase::Hover,
                pressure: 0.3,
                ..
            })
        ));
        p.tool = PointerTool::Finger;
        assert!(pen(&p, (0.0, 0.0), (100.0, 100.0)).is_none(), "fingers aren't pens");
    }

    #[test]
    fn native_touches_carry_pressure() {
        let t = |kind, pressure| match native_touch(kind, 1, (10.0, 10.0), (100.0, 100.0), pressure) {
            Some(InputEvent::Touch { pressure, .. }) => pressure,
            _ => panic!("no touch"),
        };
        assert_eq!(t(0, None), 1.0);
        assert_eq!(t(2, Some(0.5)), 0.5);
        assert_eq!(t(2, Some(0.0)), 0.01);
        assert_eq!(t(1, Some(0.7)), 0.0, "lifted");
    }

    #[test]
    fn back_and_forward_buttons() {
        assert_eq!(mouse_button(4), Some(MouseButton::X1));
        assert_eq!(mouse_button(5), Some(MouseButton::X2));
        assert_eq!(mouse_button(0), None);
        assert_eq!(
            captured_button(0, 3),
            Some(InputEvent::MouseButton {
                button: MouseButton::Right,
                pressed: true
            })
        );
        assert_eq!(captured_button(2, 1), None, "moves don't click");
        let mut events = absolute_pointer(1, 5, (1.0, 1.0), (10.0, 10.0), false);
        events.next();
        assert_eq!(
            events.next(),
            Some(InputEvent::MouseButton {
                button: MouseButton::X2,
                pressed: false
            })
        );
    }
}
