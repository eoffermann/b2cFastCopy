//! Physical memory interrogation and large-page granularity.

use windows_sys::Win32::System::Memory::GetLargePageMinimum;
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

#[derive(Debug, Clone, Copy)]
pub struct MemoryStatus {
    pub total: u64,
    pub available: u64,
}

pub fn memory_status() -> MemoryStatus {
    let mut ms: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut ms) };
    if ok == 0 {
        // Fall back to a conservative guess rather than failing startup.
        return MemoryStatus {
            total: 2 << 30,
            available: 1 << 30,
        };
    }
    MemoryStatus {
        total: ms.ullTotalPhys,
        available: ms.ullAvailPhys,
    }
}

/// Large page granularity, typically 2 MiB on x64. Zero if unsupported.
pub fn large_page_minimum() -> usize {
    unsafe { GetLargePageMinimum() }
}
