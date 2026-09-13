//! Error type carrying Win32 error codes, since diagnosing I/O problems on
//! Windows almost always comes down to the numeric code.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// A Win32 API failed. `ctx` names the call site, not just the API.
    Win32 {
        ctx: String,
        code: u32,
        msg: String,
    },
    Config(String),
}

impl Error {
    /// Capture `GetLastError` immediately after a failed call. Must be called
    /// before any other API runs, or the code will have been overwritten.
    pub fn last(ctx: impl Into<String>) -> Error {
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        Error::Win32 {
            ctx: ctx.into(),
            code,
            msg: format_message(code),
        }
    }

    pub fn config(msg: impl Into<String>) -> Error {
        Error::Config(msg.into())
    }

    pub fn code(&self) -> Option<u32> {
        match self {
            Error::Win32 { code, .. } => Some(*code),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Win32 { ctx, code, msg } => {
                if msg.is_empty() {
                    write!(f, "{ctx}: Win32 error {code}")
                } else {
                    write!(f, "{ctx}: {msg} (error {code})")
                }
            }
            Error::Config(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

fn format_message(code: u32) -> String {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        FormatMessageW, FORMAT_MESSAGE_FROM_SYSTEM, FORMAT_MESSAGE_IGNORE_INSERTS,
    };
    let mut buf = [0u16; 512];
    let len = unsafe {
        FormatMessageW(
            FORMAT_MESSAGE_FROM_SYSTEM | FORMAT_MESSAGE_IGNORE_INSERTS,
            std::ptr::null(),
            code,
            0,
            buf.as_mut_ptr(),
            buf.len() as u32,
            std::ptr::null(),
        )
    };
    if len == 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
        .trim()
        .to_string()
}
