//! Thin, honest wrappers over the Win32 calls this engine depends on.

pub mod file;
pub mod mem;
pub mod privileges;
pub mod volume;

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

/// NUL-terminated UTF-16, for APIs taking a plain string.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Encode a path for `CreateFileW`, applying the extended-length prefix.
///
/// The prefix lifts the 260-character MAX_PATH limit without requiring the
/// long-path manifest opt-in, which matters because deep trees are exactly what
/// a bulk copier meets. It also disables all path normalisation, so it may only
/// be applied to an already-absolute path.
pub fn wide_path(p: &Path) -> Vec<u16> {
    let text = p.to_string_lossy();
    let mut out: Vec<u16> = Vec::with_capacity(text.len() + 10);

    let prefixed = text.starts_with("\\\\?\\") || text.starts_with("\\\\.\\");
    let unc = !prefixed && text.starts_with("\\\\");
    let drive = !prefixed && !unc && text.len() > 2 && text.as_bytes()[1] == b':';

    if unc {
        // \\server\share becomes \\?\UNC\server\share
        out.extend("\\\\?\\UNC\\".encode_utf16());
        out.extend(OsStr::new(&text[2..]).encode_wide());
    } else {
        if drive {
            out.extend("\\\\?\\".encode_utf16());
        }
        out.extend(p.as_os_str().encode_wide());
    }
    out.push(0);
    out
}
