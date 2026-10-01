use std::mem::size_of;

use windows::Win32::Foundation::{HWND, LPARAM, POINT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON, VK_XBUTTON1, VK_XBUTTON2,
};
use windows::Win32::UI::Input::{
    GetRawInputData, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT, RAWINPUTDEVICE, RAWINPUTHEADER,
    RID_INPUT, RIDEV_INPUTSINK, RIDEV_REMOVE, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
    RegisterRawInputDevices,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

const RI_KEY_BREAK: u16 = 0x0001;
const RI_MOUSE_BUTTON_MASK: u16 = 0x03FF;
const RI_MOUSE_WHEEL: u16 = 0x0400;
const RI_MOUSE_HWHEEL: u16 = 0x0800;

#[repr(C, align(16))]
struct RawBuffer([u8; 256]);

#[derive(Clone, Copy, Debug, Default)]
pub struct InputEvents {
    pub moved: bool,
    pub button: bool,
    pub wheel: bool,
    pub key_down: bool,
}

pub fn register(hwnd: HWND) -> Result<(), String> {
    // RIDEV_NOLEGACY is intentionally absent. It would stop normal input for the session.
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        },
    ];
    unsafe { RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32) }
        .map_err(|error| error.to_string())
}

pub fn unregister() {
    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        },
        RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x06,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        },
    ];
    unsafe {
        let _ = RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32);
    }
}

pub fn parse(lparam: LPARAM) -> Option<InputEvents> {
    unsafe {
        let handle = HRAWINPUT(lparam.0 as *mut _);
        let mut buffer = RawBuffer([0; 256]);
        let mut size = buffer.0.len() as u32;
        let read = GetRawInputData(
            handle,
            RID_INPUT,
            Some(buffer.0.as_mut_ptr().cast()),
            &mut size,
            size_of::<RAWINPUTHEADER>() as u32,
        );
        if read == u32::MAX || read == 0 {
            return None;
        }
        let raw = &*buffer.0.as_ptr().cast::<RAWINPUT>();
        if raw.header.dwType == RIM_TYPEMOUSE.0 {
            let mouse = raw.data.mouse;
            let button_flags = mouse.Anonymous.Anonymous.usButtonFlags;
            return Some(InputEvents {
                button: button_flags & RI_MOUSE_BUTTON_MASK != 0,
                wheel: button_flags & (RI_MOUSE_WHEEL | RI_MOUSE_HWHEEL) != 0,
                moved: mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 != 0
                    || mouse.lLastX != 0
                    || mouse.lLastY != 0,
                key_down: false,
            });
        }
        if raw.header.dwType == RIM_TYPEKEYBOARD.0 {
            let keyboard = raw.data.keyboard;
            if keyboard.Flags & RI_KEY_BREAK == 0 {
                return Some(InputEvents {
                    key_down: true,
                    ..InputEvents::default()
                });
            }
        }
        None
    }
}

pub fn pointer_position() -> Option<POINT> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some(point)
}

pub fn any_mouse_button_down() -> bool {
    const BUTTONS: [i32; 5] = [
        VK_LBUTTON.0 as i32,
        VK_RBUTTON.0 as i32,
        VK_MBUTTON.0 as i32,
        VK_XBUTTON1.0 as i32,
        VK_XBUTTON2.0 as i32,
    ];
    BUTTONS
        .iter()
        .any(|button| unsafe { GetAsyncKeyState(*button) } < 0)
}

pub fn moved_enough(from: POINT, to: POINT, threshold: i32) -> bool {
    let dx = i64::from(to.x) - i64::from(from.x);
    let dy = i64::from(to.y) - i64::from(from.y);
    let threshold = i64::from(threshold);
    dx * dx + dy * dy >= threshold * threshold
}
