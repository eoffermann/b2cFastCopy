//! Device classification and the parameters that follow from it.
//!
//! Block size and queue depth are not global constants. The settings that
//! saturate an NVMe drive actively cost throughput on a USB hard disk, which is
//! the single most common reason a naive copier is slow on one of the two.

use crate::win::volume::{self, VolumeInfo};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceClass {
    Nvme,
    Ssd,
    Hdd,
    Unknown,
}

impl DeviceClass {
    pub fn label(&self) -> &'static str {
        match self {
            DeviceClass::Nvme => "NVMe SSD",
            DeviceClass::Ssd => "SSD",
            DeviceClass::Hdd => "HDD",
            DeviceClass::Unknown => "unknown",
        }
    }

    /// Spinning media must not be asked for concurrency: every extra in-flight
    /// request becomes another head movement.
    pub fn seeks(&self) -> bool {
        matches!(self, DeviceClass::Hdd)
    }
}

#[derive(Debug, Clone)]
pub struct DeviceProfile {
    pub class: DeviceClass,
    pub block_size: usize,
    pub queue_depth: usize,
    pub alignment: u32,
    pub volume: VolumeInfo,
}

impl DeviceProfile {
    pub fn describe(&self) -> String {
        format!(
            "{} on {} ({}, {} KiB blocks, queue depth {}, {} KiB alignment)",
            self.class.label(),
            self.volume.root,
            self.volume
                .disks
                .iter()
                .map(|d| format!("disk {d}"))
                .collect::<Vec<_>>()
                .join("+"),
            self.block_size / 1024,
            self.queue_depth,
            self.alignment / 1024,
        )
    }
}

pub fn profile(path: &Path) -> crate::error::Result<DeviceProfile> {
    let vol = volume::for_path(path)?;
    let class = classify(&vol);
    let alignment = vol.alignment();

    // Starting points before a measured ramp exists. Deliberately conservative
    // for spinning media, where being wrong is expensive.
    let (block, qd) = match class {
        DeviceClass::Nvme => (4 << 20, 32),
        DeviceClass::Ssd => (2 << 20, 12),
        DeviceClass::Hdd => (8 << 20, 2),
        DeviceClass::Unknown => (2 << 20, 8),
    };

    Ok(DeviceProfile {
        class,
        block_size: round_up(block, alignment as usize),
        queue_depth: qd,
        alignment,
        volume: vol,
    })
}

/// Random-read latency above this is taken as evidence of a moving head.
/// Spinning disks land near 10 ms; anything solid state is far below 1 ms.
const SEEK_LATENCY_THRESHOLD_MS: f64 = 3.0;

fn classify(vol: &VolumeInfo) -> DeviceClass {
    let Some(&disk) = vol.disks.first() else {
        return DeviceClass::Unknown;
    };

    let solid_state_class = |disk: u32| match volume::bus_type(disk) {
        Some(volume::BUS_NVME) => DeviceClass::Nvme,
        _ => DeviceClass::Ssd,
    };

    // Seek penalty is the authoritative signal when the device answers.
    match volume::seek_penalty(disk) {
        Some(true) => DeviceClass::Hdd,
        Some(false) => solid_state_class(disk),
        None => {
            // Plenty of USB bridges simply refuse the query, which is how an
            // 8 TB spinning disk ends up looking like an SSD. Time it instead.
            match volume::probe_random_read_ms(disk) {
                Some(ms) => {
                    crate::progress::note(format!(
                        "  disk {disk} did not report a seek penalty; measured {ms:.2} ms \
                         random read \u{2192} {}",
                        if ms > SEEK_LATENCY_THRESHOLD_MS {
                            "spinning"
                        } else {
                            "solid state"
                        }
                    ));
                    if ms > SEEK_LATENCY_THRESHOLD_MS {
                        DeviceClass::Hdd
                    } else {
                        solid_state_class(disk)
                    }
                }
                None => DeviceClass::Unknown,
            }
        }
    }
}

/// True when both paths live on the same physical spindle, which turns a copy
/// into a seek-thrash unless the pipeline serialises into alternating bursts.
pub fn shares_spindle(a: &DeviceProfile, b: &DeviceProfile) -> bool {
    a.volume.disks.iter().any(|d| b.volume.disks.contains(d))
}

pub fn round_up(v: usize, to: usize) -> usize {
    if to == 0 {
        return v;
    }
    v.div_ceil(to) * to
}
