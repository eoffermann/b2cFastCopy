//! Volume geometry and the physical device behind a path.
//!
//! Two facts drive every tuning decision: the cluster size a write must be a
//! multiple of, and whether the device underneath has a seek penalty. Neither
//! can be guessed from the drive letter.

use crate::error::{Error, Result};
use crate::win::{wide, wide_path};
use std::path::Path;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetDiskFreeSpaceW, GetVolumePathNameW, ReadFile,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, IOCTL_STORAGE_QUERY_PROPERTY,
    STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::IO::OVERLAPPED;

/// `CTL_CODE(IOCTL_VOLUME_BASE, 0, METHOD_BUFFERED, FILE_ANY_ACCESS)` where
/// IOCTL_VOLUME_BASE is 'V' (0x56). windows-sys does not export this one.
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

/// `STORAGE_PROPERTY_ID::StorageDeviceProperty`, likewise unexported.
const STORAGE_DEVICE_PROPERTY: i32 = 0;

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// Mount root, e.g. `H:\`.
    pub root: String,
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub free_bytes: u64,
    /// Physical disk numbers backing this volume (more than one if spanned).
    pub disks: Vec<u32>,
}

impl VolumeInfo {
    /// Smallest unit every unbuffered transfer must be a multiple of.
    ///
    /// Uses the cluster size rather than the sector size because it is the
    /// coarser of the two and is always itself a sector multiple, so one value
    /// satisfies both constraints. On this project's H: that is 64 KiB.
    pub fn alignment(&self) -> u32 {
        self.bytes_per_cluster.max(self.bytes_per_sector).max(4096)
    }
}

pub fn for_path(path: &Path) -> Result<VolumeInfo> {
    let w = wide_path(path);
    let mut root_buf = [0u16; 260];
    let ok =
        unsafe { GetVolumePathNameW(w.as_ptr(), root_buf.as_mut_ptr(), root_buf.len() as u32) };
    if ok == 0 {
        return Err(Error::last(format!(
            "resolve volume for {}",
            path.display()
        )));
    }
    let root = from_wide(&root_buf);

    let rootw = wide(&root);
    let (mut spc, mut bps, mut freec, mut totalc) = (0u32, 0u32, 0u32, 0u32);
    let ok =
        unsafe { GetDiskFreeSpaceW(rootw.as_ptr(), &mut spc, &mut bps, &mut freec, &mut totalc) };
    if ok == 0 {
        return Err(Error::last(format!("query geometry of {root}")));
    }

    let mut free_bytes = 0u64;
    unsafe {
        GetDiskFreeSpaceExW(
            rootw.as_ptr(),
            &mut free_bytes,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };

    Ok(VolumeInfo {
        disks: disks_for_volume(&root),
        root,
        bytes_per_sector: bps.max(512),
        bytes_per_cluster: spc.saturating_mul(bps).max(bps.max(512)),
        free_bytes,
    })
}

struct Device(HANDLE);

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Open a device path with no access rights. Query IOCTLs do not require read
/// access, which is what lets this work without elevation.
fn open_device(path: &str) -> Option<Device> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        None
    } else {
        Some(Device(h))
    }
}

fn disks_for_volume(root: &str) -> Vec<u32> {
    // GetVolumePathNameW echoes back any extended-length prefix it was given,
    // which would produce the nonsense device path \\.\\\?\C:.
    let bare = root
        .strip_prefix("\\\\?\\")
        .or_else(|| root.strip_prefix("\\\\.\\"))
        .unwrap_or(root);
    // \\.\H: with no trailing backslash.
    let trimmed = bare.trim_end_matches('\\');
    let Some(dev) = open_device(&format!("\\\\.\\{trimmed}")) else {
        return Vec::new();
    };

    // VOLUME_DISK_EXTENTS is variable length; one page covers any real layout.
    let mut buf = [0u8; 1024];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            dev.0,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            std::ptr::null(),
            0,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || returned < 8 {
        return Vec::new();
    }

    let count = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let mut disks = Vec::with_capacity(count);
    // Layout: DWORD NumberOfDiskExtents, then 4 bytes padding for 8-byte
    // alignment, then DISK_EXTENT { DWORD DiskNumber; ... } entries of 24 bytes.
    for i in 0..count.min(16) {
        let off = 8 + i * 24;
        if off + 4 > buf.len() {
            break;
        }
        disks.push(u32::from_ne_bytes([
            buf[off],
            buf[off + 1],
            buf[off + 2],
            buf[off + 3],
        ]));
    }
    disks.sort_unstable();
    disks.dedup();
    disks
}

/// STORAGE_BUS_TYPE values we care about.
pub const BUS_USB: u32 = 7;
pub const BUS_SATA: u32 = 11;
pub const BUS_NVME: u32 = 17;

/// Bus type behind a physical disk, distinguishing an NVMe drive (which wants
/// deep queues) from a USB-attached one (which does not).
pub fn bus_type(disk: u32) -> Option<u32> {
    let dev = open_device(&format!("\\\\.\\PhysicalDrive{disk}"))?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: STORAGE_DEVICE_PROPERTY,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut out = [0u8; 512];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            dev.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    // STORAGE_DEVICE_DESCRIPTOR places BusType at offset 28, after the four
    // leading DWORDs, four bytes of flags, and the four id offsets.
    if ok == 0 || returned < 32 {
        return None;
    }
    Some(u32::from_ne_bytes([out[28], out[29], out[30], out[31]]))
}

/// `Some(true)` for a device that seeks (spinning), `Some(false)` for solid
/// state, `None` when the device will not answer.
pub fn seek_penalty(disk: u32) -> Option<bool> {
    let dev = open_device(&format!("\\\\.\\PhysicalDrive{disk}"))?;
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // DEVICE_SEEK_PENALTY_DESCRIPTOR: Version, Size, BOOLEAN IncursSeekPenalty.
    let mut out = [0u8; 16];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            dev.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            out.as_mut_ptr().cast(),
            out.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || returned < 9 {
        return None;
    }
    Some(out[8] != 0)
}

/// `CTL_CODE(IOCTL_DISK_BASE, 0x17, METHOD_BUFFERED, FILE_READ_ACCESS)`.
const IOCTL_DISK_GET_LENGTH_INFO: u32 = 0x0007_405C;
const ACCESS_READ: u32 = 0x8000_0000;
const FLAG_NO_BUFFERING: u32 = 0x2000_0000;

#[repr(align(4096))]
struct AlignedBlock([u8; 4096]);

/// Median random-read latency in milliseconds, read straight from the physical
/// device.
///
/// This exists because plenty of USB bridges refuse to answer the seek-penalty
/// query, leaving an 8 TB spinning disk indistinguishable from an SSD. Latency
/// separates them unambiguously: a seek costs milliseconds, a flash lookup does
/// not.
///
/// Requires read access to the raw device, so it returns `None` unelevated —
/// in which case classification falls back to the conservative default.
pub fn probe_random_read_ms(disk: u32) -> Option<f64> {
    const READS: usize = 16;

    let w = wide(&format!("\\\\.\\PhysicalDrive{disk}"));
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            ACCESS_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FLAG_NO_BUFFERING,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        return None;
    }
    let dev = Device(h);

    // Ask how big it is so the probe seeks across the whole platter rather than
    // within one cached region.
    let mut len: i64 = 0;
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            dev.0,
            IOCTL_DISK_GET_LENGTH_INFO,
            std::ptr::null(),
            0,
            &mut len as *mut _ as *mut std::ffi::c_void,
            8,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || len <= 0 {
        return None;
    }

    let span = (len as u64).saturating_sub(1 << 20);
    let mut buf = AlignedBlock([0u8; 4096]);
    // Cheap LCG; the offsets only need to be spread out, not statistically good.
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    let mut samples = Vec::with_capacity(READS);

    for _ in 0..READS {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let offset = ((seed >> 16) % span.max(1)) & !4095;
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.Anonymous.Anonymous.Offset = offset as u32;
        ov.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;

        let mut got = 0u32;
        let start = std::time::Instant::now();
        let ok = unsafe { ReadFile(dev.0, buf.0.as_mut_ptr().cast(), 4096, &mut got, &mut ov) };
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        if ok != 0 {
            samples.push(elapsed);
        }
    }

    if samples.len() < READS / 2 {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(samples[samples.len() / 2])
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
