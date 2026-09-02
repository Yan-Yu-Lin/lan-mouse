//! uinput emulation backend (Linux only)
//!
//! Creates a virtual keyboard and a virtual mouse in the kernel via
//! `/dev/uinput`. Unlike the wayland / libei backends, events injected here
//! enter the input stack *below* the compositor, so evdev-level tools such as
//! keyd or libinput see them exactly like a physical device.
//!
//! The backend is compositor-agnostic and needs no portal. It requires write
//! access to `/dev/uinput` (typically via a udev rule or an ACL).

use async_trait::async_trait;
use evdev::{
    AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode, uinput::VirtualDevice,
};
use input_event::{Event, KeyboardEvent, PointerEvent};

use super::{
    Emulation, EmulationHandle,
    error::{EmulationError, UinputEmulationCreationError},
};

/// evdev key codes range 0..=KEY_MAX (0x2ff); register all of them so any
/// scancode the capture side sends is accepted by the kernel.
const KEY_MAX: u16 = 0x2ff;

/// one notch of a scroll wheel in REL_WHEEL_HI_RES units
const HI_RES_NOTCH: f64 = 120.;

/// pixels of continuous (touchpad) scroll that map to one wheel notch.
/// libinput uses 120 hi-res units == 15 degrees == ~one line, which macOS
/// reports as ~10 points of continuous scroll.
const PIXELS_PER_NOTCH: f64 = 10.;

pub(crate) struct UinputEmulation {
    keyboard: VirtualDevice,
    mouse: VirtualDevice,
    /// carry fractional motion between events so slow movements are not lost
    motion_remainder: (f64, f64),
    /// carry fractional hi-res scroll so small touchpad deltas accumulate
    scroll_remainder: (f64, f64),
    /// accumulated hi-res scroll since the last emitted REL_WHEEL notch
    wheel_accum: (i32, i32),
}

impl UinputEmulation {
    pub(crate) fn new() -> Result<Self, UinputEmulationCreationError> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in 1..=KEY_MAX {
            keys.insert(KeyCode::new(code));
        }
        let keyboard = VirtualDevice::builder()?
            .name("lan-mouse virtual keyboard")
            .with_keys(&keys)?
            .build()?;

        let mut buttons = AttributeSet::<KeyCode>::new();
        for code in [
            KeyCode::BTN_LEFT,
            KeyCode::BTN_RIGHT,
            KeyCode::BTN_MIDDLE,
            KeyCode::BTN_SIDE,
            KeyCode::BTN_EXTRA,
            KeyCode::BTN_FORWARD,
            KeyCode::BTN_BACK,
            KeyCode::BTN_TASK,
        ] {
            buttons.insert(code);
        }
        let mut axes = AttributeSet::<RelativeAxisCode>::new();
        for code in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            axes.insert(code);
        }
        let mouse = VirtualDevice::builder()?
            .name("lan-mouse virtual mouse")
            .with_keys(&buttons)?
            .with_relative_axes(&axes)?
            .build()?;

        log::debug!("uinput: created virtual keyboard and mouse");

        Ok(Self {
            keyboard,
            mouse,
            motion_remainder: (0., 0.),
            scroll_remainder: (0., 0.),
            wheel_accum: (0, 0),
        })
    }

    fn emit_motion(&mut self, dx: f64, dy: f64) -> std::io::Result<()> {
        let x = dx + self.motion_remainder.0;
        let y = dy + self.motion_remainder.1;
        let (ix, iy) = (x.trunc(), y.trunc());
        self.motion_remainder = (x - ix, y - iy);
        let (ix, iy) = (ix as i32, iy as i32);
        if ix == 0 && iy == 0 {
            return Ok(());
        }
        let mut events = Vec::with_capacity(2);
        if ix != 0 {
            events.push(rel(RelativeAxisCode::REL_X, ix));
        }
        if iy != 0 {
            events.push(rel(RelativeAxisCode::REL_Y, iy));
        }
        self.mouse.emit(&events)
    }

    /// emit scroll given in hi-res (1/120 notch) units.
    /// `axis`: 0 = vertical, 1 = horizontal (lan-mouse convention)
    fn emit_scroll_hi_res(&mut self, axis: u8, hi_res: f64) -> std::io::Result<()> {
        let remainder = if axis == 0 {
            &mut self.scroll_remainder.0
        } else {
            &mut self.scroll_remainder.1
        };
        let total = hi_res + *remainder;
        let whole = total.trunc();
        *remainder = total - whole;
        let whole = whole as i32;
        if whole == 0 {
            return Ok(());
        }

        // evdev convention: positive REL_WHEEL = scroll up / away from user,
        // whereas lan-mouse (wayland) axis value positive = scroll down.
        // Horizontal: positive lan-mouse = right, REL_HWHEEL positive = right.
        let (hi_res_code, wheel_code, accum, value) = if axis == 0 {
            (
                RelativeAxisCode::REL_WHEEL_HI_RES,
                RelativeAxisCode::REL_WHEEL,
                &mut self.wheel_accum.0,
                -whole,
            )
        } else {
            (
                RelativeAxisCode::REL_HWHEEL_HI_RES,
                RelativeAxisCode::REL_HWHEEL,
                &mut self.wheel_accum.1,
                whole,
            )
        };

        let mut events = vec![rel(hi_res_code, value)];
        // legacy REL_WHEEL: one event per full notch, for consumers that
        // ignore the hi-res axis
        *accum += value;
        let notches = *accum / HI_RES_NOTCH as i32;
        if notches != 0 {
            *accum -= notches * HI_RES_NOTCH as i32;
            events.push(rel(wheel_code, notches));
        }
        self.mouse.emit(&events)
    }
}

fn rel(code: RelativeAxisCode, value: i32) -> InputEvent {
    InputEvent::new(EventType::RELATIVE.0, code.0, value)
}

fn key(code: u16, state: i32) -> InputEvent {
    InputEvent::new(EventType::KEY.0, code, state)
}

#[async_trait]
impl Emulation for UinputEmulation {
    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(e) => match e {
                PointerEvent::Motion { dx, dy, .. } => self.emit_motion(dx, dy)?,
                PointerEvent::Button { button, state, .. } => {
                    let Ok(code) = u16::try_from(button) else {
                        log::warn!("uinput: button out of range: {button}");
                        return Ok(());
                    };
                    self.mouse.emit(&[key(code, state as i32)])?;
                }
                PointerEvent::Axis { axis, value, .. } => {
                    // continuous scroll in pixels / points
                    let hi_res = value / PIXELS_PER_NOTCH * HI_RES_NOTCH;
                    self.emit_scroll_hi_res(axis, hi_res)?;
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    self.emit_scroll_hi_res(axis, value as f64)?;
                }
            },
            Event::Keyboard(e) => match e {
                KeyboardEvent::Key { key: code, state, .. } => {
                    let Ok(code) = u16::try_from(code) else {
                        log::warn!("uinput: key out of range: {code}");
                        return Ok(());
                    };
                    if code > KEY_MAX {
                        log::warn!("uinput: key out of range: {code}");
                        return Ok(());
                    }
                    self.keyboard.emit(&[key(code, state as i32)])?;
                }
                // a kernel device has no notion of modifier state; consumers
                // (xkb, keyd, ...) derive it from the key events themselves.
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {
        /* devices are shared across clients */
    }

    async fn destroy(&mut self, _handle: EmulationHandle) {
        /* nothing to do, InputEmulation releases pressed keys */
    }

    async fn terminate(&mut self) {
        /* devices are destroyed on drop */
    }
}
