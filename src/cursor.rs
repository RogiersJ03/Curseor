use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, HBITMAP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, CreateIconIndirect, DestroyCursor, GetSystemMetrics, HCURSOR, HICON, ICONINFO,
    OCR_APPSTARTING, OCR_CROSS, OCR_HAND, OCR_IBEAM, OCR_NO, OCR_NORMAL, OCR_SIZEALL, OCR_SIZENESW,
    OCR_SIZENS, OCR_SIZENWSE, OCR_SIZEWE, OCR_UP, OCR_WAIT, SM_CXCURSOR, SM_CYCURSOR,
    SPI_SETCURSORS, SYSTEM_CURSOR_ID, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SetSystemCursor,
    SystemParametersInfoW,
};
use windows::core::BOOL;

use crate::config;

struct SystemCursor {
    id: SYSTEM_CURSOR_ID,
    name: &'static str,
    required: bool,
}

const CURSORS: &[SystemCursor] = &[
    SystemCursor {
        id: OCR_NORMAL,
        name: "normal",
        required: true,
    },
    SystemCursor {
        id: OCR_IBEAM,
        name: "ibeam",
        required: true,
    },
    SystemCursor {
        id: OCR_WAIT,
        name: "wait",
        required: false,
    },
    SystemCursor {
        id: OCR_CROSS,
        name: "cross",
        required: false,
    },
    SystemCursor {
        id: OCR_UP,
        name: "up",
        required: false,
    },
    SystemCursor {
        id: OCR_SIZENWSE,
        name: "size_nwse",
        required: false,
    },
    SystemCursor {
        id: OCR_SIZENESW,
        name: "size_nesw",
        required: false,
    },
    SystemCursor {
        id: OCR_SIZEWE,
        name: "size_we",
        required: false,
    },
    SystemCursor {
        id: OCR_SIZENS,
        name: "size_ns",
        required: false,
    },
    SystemCursor {
        id: OCR_SIZEALL,
        name: "size_all",
        required: false,
    },
    SystemCursor {
        id: OCR_NO,
        name: "no",
        required: false,
    },
    SystemCursor {
        id: OCR_HAND,
        name: "hand",
        required: false,
    },
    SystemCursor {
        id: OCR_APPSTARTING,
        name: "app_starting",
        required: false,
    },
];

pub struct HideSuccess {
    pub best_effort_failures: Vec<(&'static str, String)>,
}

pub enum HideError {
    Marker(String),
    Blank(String),
    Required {
        name: &'static str,
        error: String,
        /// True when this attempt put the user's cursors back.
        restored: bool,
    },
}

impl std::fmt::Display for HideError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Marker(error) => write!(formatter, "could not write the marker: {error}"),
            Self::Blank(error) => write!(formatter, "could not create a blank cursor: {error}"),
            Self::Required { name, error, .. } => write!(formatter, "{name}: {error}"),
        }
    }
}

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        restore_for_panic();
    }));
}

pub fn repair_if_unclean() {
    let Some(path) = marker_path() else {
        return;
    };
    if !path.is_file() {
        return;
    }
    tracing::info!("restoring the pointer after an unclean shutdown");
    if reload_system_cursors() {
        let _ = fs::remove_file(path);
    } else {
        tracing::error!("could not restore the pointer; the marker was left in place");
    }
}

pub fn hide_system_cursors(already_hidden: bool) -> Result<HideSuccess, HideError> {
    write_marker().map_err(HideError::Marker)?;
    let template = match create_blank() {
        Ok(cursor) => cursor,
        Err(error) => {
            // A re-apply has not replaced anything yet. The earlier blank cursors
            // are still installed, so the marker has to stay for the next start.
            if !already_hidden {
                delete_marker();
            }
            return Err(HideError::Blank(error));
        }
    };

    let mut best_effort_failures = Vec::new();
    for cursor in CURSORS {
        match replace_one(template, cursor.id) {
            Ok(()) => {}
            Err(error) if cursor.required => {
                let restored = reload_system_cursors();
                if restored {
                    delete_marker();
                }
                destroy_cursor(template);
                return Err(HideError::Required {
                    name: cursor.name,
                    error,
                    restored,
                });
            }
            Err(error) => best_effort_failures.push((cursor.name, error)),
        }
    }
    destroy_cursor(template);
    Ok(HideSuccess {
        best_effort_failures,
    })
}

/// Reloads the user's cursor scheme. Does not write the scheme back.
pub fn reload_system_cursors() -> bool {
    // SAFETY: SPI_SETCURSORS takes a null parameter and a zero update flag.
    // It broadcasts WM_SETTINGCHANGE, so callers must not be holding `&mut App`.
    unsafe {
        SystemParametersInfoW(
            SPI_SETCURSORS,
            0,
            None,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
    }
}

pub fn marker_exists() -> bool {
    marker_path().is_some_and(|path| path.is_file())
}

pub fn delete_marker() {
    if let Some(path) = marker_path() {
        let _ = fs::remove_file(path);
    }
}

fn restore_for_panic() {
    let Some(path) = marker_path() else {
        return;
    };
    if !path.is_file() {
        return;
    }
    if reload_system_cursors() {
        let _ = fs::remove_file(path);
    }
}

fn write_marker() -> Result<(), String> {
    let path = marker_path().ok_or_else(|| "settings directory is not available".to_string())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|error| error.to_string())?;
    file.write_all(b"hidden")
        .map_err(|error| error.to_string())?;
    // The marker has to reach disk before any system cursor is replaced.
    file.sync_all().map_err(|error| error.to_string())?;
    Ok(())
}

fn marker_path() -> Option<PathBuf> {
    config::roaming_dir()
        .ok()
        .map(|dir| dir.join("Curseor").join("hidden.marker"))
}

fn replace_one(template: HCURSOR, id: SYSTEM_CURSOR_ID) -> Result<(), String> {
    // CopyCursor is a macro for CopyIcon. SetSystemCursor destroys the copy.
    // SAFETY: `template` is the blank cursor this process created and still owns.
    // CopyIcon returns a new handle. The template stays valid.
    let copy = unsafe { CopyIcon(HICON(template.0)) }.map_err(|error| error.to_string())?;
    // SAFETY: `copy` is the new handle from CopyIcon. SetSystemCursor takes ownership of it.
    unsafe { SetSystemCursor(HCURSOR(copy.0), id) }.map_err(|error| error.to_string())
}

fn destroy_cursor(cursor: HCURSOR) {
    // SAFETY: `cursor` is the blank cursor this process created. This is its only destroy.
    unsafe {
        let _ = DestroyCursor(cursor);
    }
}

/// A fully transparent cursor. An AND mask of zeros would draw a black square.
fn create_blank() -> Result<HCURSOR, String> {
    // SAFETY: SM_CXCURSOR and SM_CYCURSOR are fixed metric indexes and take no pointers.
    let (width, height) = unsafe {
        (
            GetSystemMetrics(SM_CXCURSOR).max(1),
            GetSystemMetrics(SM_CYCURSOR).max(1),
        )
    };
    let color = create_color_bitmap(width, height)?;
    let mask = match create_mask_bitmap(width, height) {
        Ok(mask) => mask,
        Err(error) => {
            delete_bitmap(color);
            return Err(error);
        }
    };
    let info = ICONINFO {
        fIcon: BOOL(0),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color,
    };
    // SAFETY: `info` is a live ICONINFO. Both bitmaps stay alive until after this call.
    // CreateIconIndirect copies them; the originals are deleted below.
    let created = unsafe { CreateIconIndirect(&info) };
    delete_bitmap(color);
    delete_bitmap(mask);
    let icon = created.map_err(|error| error.to_string())?;
    Ok(HCURSOR(icon.0))
}

fn create_color_bitmap(width: i32, height: i32) -> Result<HBITMAP, String> {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            biSizeImage: (width as u32)
                .saturating_mul(height as u32)
                .saturating_mul(4),
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [windows::Win32::Graphics::Gdi::RGBQUAD::default()],
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `info` is a live BITMAPINFO. `bits` is a live out-parameter. The API
    // allocates the pixel buffer and sets `bits` to it. No file mapping is used.
    let bitmap = unsafe { CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) }
        .map_err(|error| error.to_string())?;
    if bits.is_null() {
        delete_bitmap(bitmap);
        return Err("color bitmap has no pixel buffer".into());
    }
    // SAFETY: CreateDIBSection set `bits` to a 32-bit DIB of `width` by `height`.
    // Each row is already DWORD-aligned, so the byte count is width * height * 4.
    unsafe {
        std::ptr::write_bytes(
            bits.cast::<u8>(),
            0,
            (width as usize)
                .saturating_mul(height as usize)
                .saturating_mul(4),
        );
    }
    Ok(bitmap)
}

fn create_mask_bitmap(width: i32, height: i32) -> Result<HBITMAP, String> {
    let stride = ((width as usize).saturating_add(15) / 16) * 2;
    let bytes = vec![0xFFu8; stride.saturating_mul(height as usize)];
    // SAFETY: `bytes` is a packed 1-bit mask. CreateBitmap copies it before returning.
    let bitmap = unsafe { CreateBitmap(width, height, 1, 1, Some(bytes.as_ptr().cast())) };
    if bitmap.is_invalid() {
        return Err("could not create the cursor mask".into());
    }
    Ok(bitmap)
}

fn delete_bitmap(bitmap: HBITMAP) {
    if bitmap.is_invalid() {
        return;
    }
    // SAFETY: `bitmap` is a GDI bitmap this process created. This is its only delete.
    unsafe {
        let _ = DeleteObject(windows::Win32::Graphics::Gdi::HGDIOBJ(bitmap.0));
    }
}
