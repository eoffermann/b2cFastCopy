//! Thin, honest wrappers over the Win32 calls this engine depends on.

pub mod file;
pub mod mem;
pub mod privileges;
pub mod volume;

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::GetFullPathNameW;

/// NUL-terminated UTF-16, for APIs taking a plain string.
pub fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Resolve a path the way Win32 itself would: make it absolute, expand `.` and
/// `..`, convert forward slashes to backslashes, and collapse duplicate
/// separators.
///
/// This must run before [`wide_path`] applies the extended-length prefix,
/// because that prefix switches off precisely this normalisation. A destination
/// of `G:\.` is the case that motivated it: perfectly ordinary to type, and
/// `\\?\G:\.\file` asks the object manager for a directory literally named `.`,
/// which fails with ERROR_PATH_NOT_FOUND on every single file.
pub fn full_path(p: &Path) -> std::io::Result<PathBuf> {
    let input: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();

    // First call sizes the buffer; the result includes the terminator.
    let needed = unsafe {
        GetFullPathNameW(
            input.as_ptr(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut buf = vec![0u16; needed as usize];
    // Second call returns the length written, excluding the terminator.
    let written = unsafe {
        GetFullPathNameW(
            input.as_ptr(),
            buf.len() as u32,
            buf.as_mut_ptr(),
            std::ptr::null_mut(),
        )
    };
    if written == 0 || written as usize > buf.len() {
        return Err(std::io::Error::last_os_error());
    }
    buf.truncate(written as usize);
    Ok(PathBuf::from(OsString::from_wide(&buf)))
}

/// Whether a path is already in the exact form the extended-length prefix
/// requires: backslash separators only, and no `.`, `..` or empty components.
fn is_canonical(text: &str) -> bool {
    if text.contains('/') {
        return false;
    }
    // Skip the drive component; check the rest. A single trailing separator is
    // fine, so an empty final component is allowed.
    let rest = &text[2..];
    let parts: Vec<&str> = rest.split('\\').collect();
    for (i, part) in parts.iter().enumerate() {
        if *part == "." || *part == ".." {
            return false;
        }
        // An empty part is a doubled separator unless it is the leading one
        // (from the root backslash) or a single trailing one.
        if part.is_empty() && i != 0 && i != parts.len() - 1 {
            return false;
        }
    }
    true
}

/// Encode a path for `CreateFileW`, applying the extended-length prefix.
///
/// The prefix lifts the 260-character MAX_PATH limit without requiring the
/// long-path manifest opt-in, which matters because deep trees are exactly what
/// a bulk copier meets.
///
/// It is applied only to a path that is already canonical. Anything else is
/// passed through unprefixed so Win32 can normalise it normally: losing long
/// path support on an odd path is a far better failure than refusing to open it
/// at all. Callers should run roots through [`full_path`] first, which makes
/// the prefix apply in practice.
pub fn wide_path(p: &Path) -> Vec<u16> {
    let text = p.to_string_lossy();
    let mut out: Vec<u16> = Vec::with_capacity(text.len() + 10);

    let prefixed = text.starts_with("\\\\?\\") || text.starts_with("\\\\.\\");
    let unc = !prefixed && text.starts_with("\\\\");
    let drive =
        !prefixed && !unc && text.len() > 2 && text.as_bytes()[1] == b':' && is_canonical(&text);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(v: &[u16]) -> String {
        let end = v.iter().position(|&c| c == 0).unwrap_or(v.len());
        String::from_utf16_lossy(&v[..end])
    }

    #[test]
    fn canonical_drive_paths_get_the_prefix() {
        assert_eq!(
            decode(&wide_path(Path::new("C:\\dir\\file.bin"))),
            "\\\\?\\C:\\dir\\file.bin"
        );
    }

    #[test]
    fn non_canonical_paths_are_left_unprefixed() {
        // These would each fail to open if the prefix were applied, because it
        // disables the normalisation they rely on.
        for p in [
            "C:\\dir\\.\\file.bin",
            "C:\\dir\\..\\file.bin",
            "C:/dir/file.bin",
            "G:\\.",
        ] {
            let encoded = decode(&wide_path(Path::new(p)));
            assert!(
                !encoded.starts_with("\\\\?\\"),
                "{p} must not be prefixed, got {encoded}"
            );
        }
    }

    #[test]
    fn full_path_normalises_away_the_hazards() {
        let dot = full_path(Path::new("C:\\Windows\\.")).unwrap();
        assert_eq!(dot, PathBuf::from("C:\\Windows\\"));

        let parent = full_path(Path::new("C:\\Windows\\System32\\..")).unwrap();
        assert_eq!(parent, PathBuf::from("C:\\Windows"));

        let slashes = full_path(Path::new("C:/Windows/System32")).unwrap();
        assert_eq!(slashes, PathBuf::from("C:\\Windows\\System32"));
    }

    #[test]
    fn normalised_paths_then_qualify_for_the_prefix() {
        // The pairing that matters: full_path first, wide_path second.
        let normalised = full_path(Path::new("C:\\Windows\\.")).unwrap();
        assert!(decode(&wide_path(&normalised)).starts_with("\\\\?\\"));
    }
}
