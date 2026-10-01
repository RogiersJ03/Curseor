use std::ffi::c_void;
use std::mem::size_of;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, LPARAM, LocalFree,
    SetLastError, WIN32_ERROR, WPARAM,
};
use windows::Win32::Security::Authorization::{
    EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SetEntriesInAclW, TRUSTEE_IS_SID,
    TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows::Win32::Security::{
    ACL, GetTokenInformation, InitializeSecurityDescriptor, NO_INHERITANCE, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SetSecurityDescriptorDacl, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, MUTEX_ALL_ACCESS, OpenProcessToken,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_APP};
use windows::core::PWSTR;

pub const WM_APP_ALREADY_RUNNING: u32 = WM_APP + 1;

const WINDOW_CLASS: windows::core::PCWSTR = windows::core::w!("Curseor");

pub struct SingleInstance {
    mutex: HANDLE,
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // SAFETY: `mutex` is the handle returned by CreateMutexW for this instance.
        // Drop runs once. The handle is not a pseudo-handle.
        unsafe {
            let _ = CloseHandle(self.mutex);
        }
    }
}

pub enum Instance {
    Primary(SingleInstance),
    Secondary,
}

pub fn acquire() -> Result<Instance, String> {
    let sid_buffer = current_user_sid()?;
    // SAFETY: `sid_buffer` is 8-aligned and at least as large as the TOKEN_USER
    // GetTokenInformation wrote at the front. The SID bytes it points at live in the
    // same buffer, which stays allocated until this function returns. SetEntriesInAclW
    // copies that SID before it returns.
    let user = unsafe { &*sid_buffer.as_ptr().cast::<TOKEN_USER>() };
    let access = [explicit_access(user.User.Sid)];
    let mut acl: *mut ACL = std::ptr::null_mut();
    // SAFETY: `access` describes the current user's SID. The new ACL is allocated by
    // the API and freed by `acl_guard`. A null old ACL starts a new list.
    let acl_error = unsafe { SetEntriesInAclW(Some(&access), None, &mut acl) };
    if !acl_error.is_ok() {
        return Err(format!(
            "could not build the instance mutex DACL: {acl_error:?}"
        ));
    }
    let acl_guard = AclGuard(acl);

    let mut descriptor = SECURITY_DESCRIPTOR::default();
    // SAFETY: `descriptor` is a live SECURITY_DESCRIPTOR. Revision 1 is
    // SECURITY_DESCRIPTOR_REVISION. `acl` stays allocated until after CreateMutexW,
    // which copies the descriptor during the call.
    unsafe {
        InitializeSecurityDescriptor(
            PSECURITY_DESCRIPTOR(&mut descriptor as *mut SECURITY_DESCRIPTOR as *mut c_void),
            1,
        )
        .map_err(|error| format!("could not initialize the security descriptor: {error}"))?;
        SetSecurityDescriptorDacl(
            PSECURITY_DESCRIPTOR(&mut descriptor as *mut SECURITY_DESCRIPTOR as *mut c_void),
            true,
            Some(acl.cast_const()),
            false,
        )
        .map_err(|error| format!("could not set the instance mutex DACL: {error}"))?;
    }

    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: &mut descriptor as *mut SECURITY_DESCRIPTOR as *mut c_void,
        bInheritHandle: false.into(),
    };
    // CreateMutex reports an existing object through the last-error code, so clear it first.
    // The windows wrapper leaves that code intact when the handle is valid.
    // SAFETY: SetLastError only writes this thread's last-error slot.
    unsafe { SetLastError(WIN32_ERROR(0)) };
    // SAFETY: `attributes` points at a live SECURITY_ATTRIBUTES whose descriptor and ACL
    // stay alive for this call. The name is a NUL-terminated literal.
    let mutex = unsafe {
        CreateMutexW(
            Some(&raw const attributes),
            true,
            windows::core::w!("Local\\Curseor"),
        )
    };
    // SAFETY: GetLastError reads this thread's last-error slot and takes no pointers.
    let already_running = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    drop(acl_guard);
    let mutex = mutex.map_err(|error| format!("could not create the instance mutex: {error}"))?;
    if already_running {
        // SAFETY: `mutex` is the valid handle just returned for the existing object.
        // This path does not keep it.
        unsafe {
            let _ = CloseHandle(mutex);
        }
        return Ok(Instance::Secondary);
    }
    Ok(Instance::Primary(SingleInstance { mutex }))
}

pub fn signal_primary() {
    let started = Instant::now();
    loop {
        // SAFETY: WINDOW_CLASS is a NUL-terminated class name. FindWindowW does not retain it.
        if let Ok(hwnd) = unsafe { FindWindowW(WINDOW_CLASS, None) }
            && !hwnd.is_invalid()
            // SAFETY: `hwnd` was just found for this class. The message is queued, not sent.
            && unsafe {
                PostMessageW(Some(hwnd), WM_APP_ALREADY_RUNNING, WPARAM(0), LPARAM(0)).is_ok()
            }
        {
            return;
        }
        if started.elapsed() >= Duration::from_millis(500) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn current_user_sid() -> Result<Vec<u64>, String> {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess is this process's pseudo-handle and must not be closed.
    // TOKEN_QUERY is the access used below. `token` is a live out-parameter.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|error| format!("could not open the process token: {error}"))?;

    let mut needed = 0u32;
    // SAFETY: a null buffer with length 0 is the documented size query. `needed` is live.
    // `token` is the handle just opened.
    unsafe {
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
    }
    if needed == 0 {
        // SAFETY: `token` is the open process token and has not been closed yet.
        unsafe {
            let _ = CloseHandle(token);
        }
        return Err("could not read the current user".into());
    }
    let mut buffer = vec![0u64; needed.div_ceil(8) as usize];
    // SAFETY: `buffer` is writable, 8-aligned, and at least `needed` bytes.
    // GetTokenInformation writes a TOKEN_USER and its SID into that allocation.
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            (buffer.len() * size_of::<u64>()) as u32,
            &mut needed,
        )
    };
    // SAFETY: `token` is still the open process token. This is its only close.
    unsafe {
        let _ = CloseHandle(token);
    }
    read.map_err(|error| format!("could not read the current user: {error}"))?;
    Ok(buffer)
}

fn explicit_access(sid: PSID) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: MUTEX_ALL_ACCESS.0,
        grfAccessMode: GRANT_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: PWSTR(sid.0 as *mut u16),
        },
    }
}

struct AclGuard(*mut ACL);

impl Drop for AclGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is the ACL allocated by SetEntriesInAclW. LocalFree is the
            // matching free, and Drop runs once.
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0.cast())));
            }
        }
    }
}
