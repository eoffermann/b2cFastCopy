//! Token privilege probing and activation.
//!
//! A privilege present in the token but reported `Disabled` is usable: it only
//! needs `AdjustTokenPrivileges` to turn on. Treating `Disabled` as unavailable
//! is the common mistake, and would silently forfeit the `SetFileValidData`
//! fast path on every elevated run.

use crate::win::wide;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, GetTokenInformation, LookupPrivilegeValueW, TokenElevation,
    LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub const SE_MANAGE_VOLUME: &str = "SeManageVolumePrivilege";
pub const SE_LOCK_MEMORY: &str = "SeLockMemoryPrivilege";

#[derive(Debug, Clone, Copy, Default)]
pub struct Privileges {
    pub elevated: bool,
    /// `SetFileValidData` is usable: NTFS zero-fill can be skipped.
    pub manage_volume: bool,
    /// Large pages are usable for the arena.
    pub lock_memory: bool,
}

struct Token(HANDLE);

impl Drop for Token {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn open_token() -> Option<Token> {
    let mut h: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut h,
        )
    };
    if ok == 0 {
        None
    } else {
        Some(Token(h))
    }
}

/// Probe and immediately enable what is available. Returns what was actually
/// obtained, so callers report capability rather than assuming it.
pub fn acquire() -> Privileges {
    let token = match open_token() {
        Some(t) => t,
        None => return Privileges::default(),
    };
    Privileges {
        elevated: is_elevated(&token),
        manage_volume: enable(&token, SE_MANAGE_VOLUME),
        lock_memory: enable(&token, SE_LOCK_MEMORY),
    }
}

fn is_elevated(token: &Token) -> bool {
    let mut elev: TOKEN_ELEVATION = unsafe { std::mem::zeroed() };
    let mut ret = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            &mut elev as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret,
        )
    };
    ok != 0 && elev.TokenIsElevated != 0
}

fn enable(token: &Token, name: &str) -> bool {
    let wname = wide(name);
    let mut luid = unsafe { std::mem::zeroed() };
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), wname.as_ptr(), &mut luid) } == 0 {
        return false;
    }
    let tp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    let ok = unsafe {
        AdjustTokenPrivileges(
            token.0,
            0,
            &tp,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // AdjustTokenPrivileges reports success even when it changed nothing; the
    // real answer is in GetLastError.
    ok != 0 && unsafe { GetLastError() } == ERROR_SUCCESS
}
