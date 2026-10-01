use std::mem::{align_of, size_of};

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
    // SAFETY: both device structs are live. The byte size matches RAWINPUTDEVICE.
    // hwndTarget is the Curseor window.
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
    // SAFETY: removal uses a null target, which RegisterRawInputDevices requires for
    // RIDEV_REMOVE. The device list is live and the byte size matches RAWINPUTDEVICE.
    unsafe {
        let _ = RegisterRawInputDevices(&devices, size_of::<RAWINPUTDEVICE>() as u32);
    }
}

pub fn parse(lparam: LPARAM) -> Option<InputEvents> {
    let raw = read_raw_input(lparam)?;
    if raw.header.dwType == RIM_TYPEMOUSE.0 {
        let mouse = mouse_sample(&raw);
        return Some(InputEvents {
            button: mouse.button_flags & RI_MOUSE_BUTTON_MASK != 0,
            wheel: mouse.button_flags & (RI_MOUSE_WHEEL | RI_MOUSE_HWHEEL) != 0,
            moved: mouse.flags & MOUSE_MOVE_ABSOLUTE.0 != 0 || mouse.x != 0 || mouse.y != 0,
            key_down: false,
        });
    }
    if raw.header.dwType == RIM_TYPEKEYBOARD.0 && key_is_down(&raw) {
        return Some(InputEvents {
            key_down: true,
            ..InputEvents::default()
        });
    }
    None
}

fn read_raw_input(lparam: LPARAM) -> Option<RAWINPUT> {
    const _: () = assert!(size_of::<RAWINPUT>() <= 256 && align_of::<RAWINPUT>() <= 16);
    // SAFETY: WM_INPUT's lParam is an HRAWINPUT for this message. The buffer is 16-byte
    // aligned, zero-initialized, and larger than RAWINPUT. A failed call returns u32::MAX
    // and does not claim the buffer holds a packet. On success the API wrote `read` bytes
    // and the rest of the buffer stays zero, so reading a full RAWINPUT is in bounds.
    unsafe {
        let handle = HRAWINPUT(lparam.0 as *mut std::ffi::c_void);
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
        Some(buffer.0.as_ptr().cast::<RAWINPUT>().read())
    }
}

struct MouseSample {
    button_flags: u16,
    flags: u16,
    x: i32,
    y: i32,
}

fn mouse_sample(raw: &RAWINPUT) -> MouseSample {
    // SAFETY: the caller checked dwType == RIM_TYPEMOUSE, so the mouse member is active.
    unsafe {
        let mouse = raw.data.mouse;
        MouseSample {
            button_flags: mouse.Anonymous.Anonymous.usButtonFlags,
            flags: mouse.usFlags.0,
            x: mouse.lLastX,
            y: mouse.lLastY,
        }
    }
}

fn key_is_down(raw: &RAWINPUT) -> bool {
    // SAFETY: the caller checked dwType == RIM_TYPEKEYBOARD, so the keyboard member is active.
    unsafe { raw.data.keyboard.Flags & RI_KEY_BREAK == 0 }
}

pub fn pointer_position() -> Option<POINT> {
    let mut point = POINT::default();
    // SAFETY: `point` is a live POINT. GetCursorPos writes the cursor position and keeps no pointer.
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
    BUTTONS.iter().any(|button| {
        // SAFETY: GetAsyncKeyState only reads one virtual-key state. These five values
        // are the mouse-button codes.
        let state = unsafe { GetAsyncKeyState(*button) };
        state < 0
    })
}

pub fn moved_enough(from: POINT, to: POINT, threshold: i32) -> bool {
    let dx = i64::from(to.x) - i64::from(from.x);
    let dy = i64::from(to.y) - i64::from(from.y);
    let threshold = i64::from(threshold);
    dx * dx + dy * dy >= threshold * threshold
}
