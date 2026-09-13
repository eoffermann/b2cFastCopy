//! File handles with positioned (offset-carrying) reads and writes.
//!
//! Handles are opened *synchronously* but every read and write supplies its own
//! `OVERLAPPED` carrying an explicit offset. That keeps the call blocking — no
//! completion plumbing — while removing any dependence on a shared file
//! pointer, so chunks can be issued in any order.
//!
//! The catch, and the reason [`Copier`](crate::engine) opens one handle per
//! worker rather than sharing one: Windows serialises I/O on a *synchronous*
//! file object. Concurrent calls on a single handle would queue behind each
//! other and collapse the queue depth to one, which is the exact failure this
//! engine exists to avoid.

use crate::error::{Error, Result};
use crate::win::wide_path;
use std::path::Path;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_HANDLE_EOF, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileSizeEx, ReadFile, SetFileInformationByHandle, SetFileTime,
    SetFileValidData, WriteFile, CREATE_ALWAYS, FILE_ALLOCATION_INFO, FILE_END_OF_FILE_INFO,
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

/// `FileAllocationInfo` / `FileEndOfFileInfo` from FILE_INFO_BY_HANDLE_CLASS.
const FILE_ALLOCATION_INFO_CLASS: i32 = 5;
const FILE_END_OF_FILE_INFO_CLASS: i32 = 6;

pub struct File {
    h: HANDLE,
}

// Sound because every operation builds its own OVERLAPPED and mutates no shared
// state. See the module note on why we still open one handle per worker.
unsafe impl Send for File {}
unsafe impl Sync for File {}

impl Drop for File {
    fn drop(&mut self) {
        if !self.h.is_null() && self.h != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.h) };
        }
    }
}

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// Raw `GENERIC_READ` / `GENERIC_WRITE`.
///
/// The composite `FILE_GENERIC_WRITE` mask must NOT be used here: it includes
/// `FILE_APPEND_DATA` (0x0004), and append access is rejected outright when
/// combined with `FILE_FLAG_NO_BUFFERING`, failing with ERROR_INVALID_PARAMETER
/// before the file is ever touched. The raw generic rights are mapped by the
/// filesystem after that validation, so they are accepted.
const ACCESS_READ: u32 = 0x8000_0000;
const ACCESS_WRITE: u32 = 0x4000_0000;

impl File {
    fn create(path: &Path, access: u32, disposition: u32, flags: u32, ctx: &str) -> Result<File> {
        let w = wide_path(path);
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                SHARE_ALL,
                std::ptr::null(),
                disposition,
                flags,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            return Err(Error::last(format!("{ctx} {}", path.display())));
        }
        Ok(File { h })
    }

    /// Source handle bypassing the system cache. Used for bulk files.
    pub fn open_read_unbuffered(path: &Path) -> Result<File> {
        Self::create(
            path,
            ACCESS_READ,
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING,
            "open",
        )
    }

    /// Source handle using the cache, for small files where the cache helps.
    pub fn open_read_buffered(path: &Path) -> Result<File> {
        Self::create(
            path,
            ACCESS_READ,
            OPEN_EXISTING,
            FILE_FLAG_SEQUENTIAL_SCAN,
            "open",
        )
    }

    /// Destination handle bypassing the system cache.
    ///
    /// Read access is requested alongside write because `SetFileValidData`
    /// requires it.
    pub fn create_write_unbuffered(path: &Path) -> Result<File> {
        Self::create(
            path,
            ACCESS_READ | ACCESS_WRITE,
            CREATE_ALWAYS,
            FILE_FLAG_NO_BUFFERING,
            "create",
        )
    }

    pub fn create_write_buffered(path: &Path) -> Result<File> {
        Self::create(
            path,
            ACCESS_WRITE,
            CREATE_ALWAYS,
            FILE_FLAG_SEQUENTIAL_SCAN,
            "create",
        )
    }

    /// Additional unbuffered write handle to a file that already exists.
    ///
    /// Workers use this rather than `CREATE_ALWAYS`, which would truncate the
    /// file out from under every other worker already writing it.
    pub fn open_write_unbuffered(path: &Path) -> Result<File> {
        Self::create(
            path,
            ACCESS_WRITE,
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING,
            "open for write",
        )
    }

    pub fn open_existing_write(path: &Path) -> Result<File> {
        Self::create(path, ACCESS_WRITE, OPEN_EXISTING, 0, "open for update")
    }

    pub fn raw(&self) -> HANDLE {
        self.h
    }

    fn overlapped(offset: u64) -> OVERLAPPED {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.Anonymous.Anonymous.Offset = offset as u32;
        ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        ov
    }

    /// Read at an absolute offset. Returns bytes read; 0 means end of file.
    ///
    /// # Safety
    /// `buf` must point to at least `len` writable bytes, and for an unbuffered
    /// handle must be sector-aligned with `len` and `offset` sector multiples.
    pub unsafe fn read_at(&self, buf: *mut u8, len: u32, offset: u64) -> Result<u32> {
        let mut ov = Self::overlapped(offset);
        let mut got: u32 = 0;
        let ok = ReadFile(self.h, buf.cast(), len, &mut got, &mut ov);
        if ok == 0 {
            let code = GetLastError();
            // Reading exactly up to EOF on an unbuffered handle reports this
            // rather than a short read.
            if code == ERROR_HANDLE_EOF {
                return Ok(0);
            }
            return Err(Error::Win32 {
                ctx: format!("read at offset {offset}"),
                code,
                msg: String::new(),
            });
        }
        Ok(got)
    }

    /// Write at an absolute offset.
    ///
    /// # Safety
    /// Same alignment contract as [`File::read_at`].
    pub unsafe fn write_at(&self, buf: *const u8, len: u32, offset: u64) -> Result<u32> {
        let mut ov = Self::overlapped(offset);
        let mut put: u32 = 0;
        let ok = WriteFile(self.h, buf.cast(), len, &mut put, &mut ov);
        if ok == 0 {
            return Err(Error::last(format!("write at offset {offset}")));
        }
        Ok(put)
    }

    pub fn size(&self) -> Result<u64> {
        let mut n: i64 = 0;
        let ok = unsafe { GetFileSizeEx(self.h, &mut n) };
        if ok == 0 {
            return Err(Error::last("query file size"));
        }
        Ok(n as u64)
    }

    /// Reserve space up front so the destination is laid out contiguously and
    /// the MFT is not updated on every extension.
    pub fn preallocate(&self, bytes: u64) -> Result<()> {
        let info = FILE_ALLOCATION_INFO {
            AllocationSize: bytes as i64,
        };
        let ok = unsafe {
            SetFileInformationByHandle(
                self.h,
                FILE_ALLOCATION_INFO_CLASS,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<FILE_ALLOCATION_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(Error::last("preallocate"));
        }
        Ok(())
    }

    /// Set the true logical length.
    ///
    /// This is what rescues an unbuffered tail write: the last chunk is written
    /// rounded up to a sector multiple, then trimmed back to the real size here.
    /// Skipping it leaves the file padded with whatever the rounding wrote.
    pub fn set_len(&self, bytes: u64) -> Result<()> {
        let info = FILE_END_OF_FILE_INFO {
            EndOfFile: bytes as i64,
        };
        let ok = unsafe {
            SetFileInformationByHandle(
                self.h,
                FILE_END_OF_FILE_INFO_CLASS,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<FILE_END_OF_FILE_INFO>() as u32,
            )
        };
        if ok == 0 {
            return Err(Error::last("set end of file"));
        }
        Ok(())
    }

    /// Declare the preallocated range valid without zero-filling it.
    ///
    /// Saves a full pass over the file, but exposes previously-deleted disk
    /// contents in any region not subsequently overwritten. Callers must hold
    /// SeManageVolumePrivilege and must overwrite the whole range.
    pub fn set_valid_data(&self, bytes: u64) -> Result<()> {
        let ok = unsafe { SetFileValidData(self.h, bytes as i64) };
        if ok == 0 {
            return Err(Error::last("set valid data"));
        }
        Ok(())
    }

    pub fn set_times(&self, created: i64, accessed: i64, written: i64) -> Result<()> {
        use windows_sys::Win32::Foundation::FILETIME;
        let ft = |v: i64| FILETIME {
            dwLowDateTime: v as u32,
            dwHighDateTime: (v >> 32) as u32,
        };
        let (c, a, w) = (ft(created), ft(accessed), ft(written));
        let ok = unsafe { SetFileTime(self.h, &c, &a, &w) };
        if ok == 0 {
            return Err(Error::last("set file times"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;

    /// Regression guard for the access mask.
    ///
    /// Using `FILE_GENERIC_WRITE` here fails every destination open with a bare
    /// ERROR_INVALID_PARAMETER that names nothing, so this pins down both
    /// halves: the composite mask really does carry FILE_APPEND_DATA and really
    /// is rejected, and ours is not and is not.
    #[test]
    fn append_access_is_rejected_with_unbuffered_io() {
        const FILE_APPEND_DATA: u32 = 0x0004;
        assert_eq!(
            FILE_GENERIC_WRITE & FILE_APPEND_DATA,
            FILE_APPEND_DATA,
            "FILE_GENERIC_WRITE is expected to include FILE_APPEND_DATA"
        );
        assert_eq!(
            ACCESS_WRITE & FILE_APPEND_DATA,
            0,
            "our write mask must not"
        );

        let dir = std::env::temp_dir().join("b2fc-mask-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("probe-{}.bin", std::process::id()));

        assert!(
            File::create_write_unbuffered(&path).is_ok(),
            "the chosen mask must open an unbuffered destination"
        );

        let rejected = File::create(
            &path,
            FILE_GENERIC_WRITE,
            CREATE_ALWAYS,
            FILE_FLAG_NO_BUFFERING,
            "probe",
        );
        assert_eq!(
            rejected.err().and_then(|e| e.code()),
            Some(87),
            "the composite mask should still be refused; if this changes, the \
             workaround can be revisited"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
