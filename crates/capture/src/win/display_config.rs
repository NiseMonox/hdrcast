//! Windows CCD (Connecting and Configuring Displays) queries: maps a GDI device name (`\\.\DISPLAY1`) to the
//! monitor's friendly name and refresh rate.

use std::collections::HashMap;

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};

use super::util::wide_to_string;

/// What CCD knows about one active display path.
#[derive(Debug, Clone, Default)]
pub(crate) struct MonitorEntry {
    pub friendly_name: String,
    pub refresh_rate: f64,
}

/// The key GDI device names are looked up by (they compare case-insensitively).
pub(crate) fn device_key(gdi_device_name: &str) -> String {
    gdi_device_name.to_lowercase()
}

/// Returns the active display paths keyed by [`device_key`] of the GDI device name. Never fails: on any error
/// the caller simply gets fewer (or no) friendly names, which only affects labels.
pub(crate) fn query_active_monitors() -> HashMap<String, MonitorEntry> {
    let mut map = HashMap::new();
    let Some(paths) = query_paths() else {
        return map;
    };

    for path in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        source.header.adapterId = path.sourceInfo.adapterId;
        source.header.id = path.sourceInfo.id;
        // SAFETY: the packet starts with its header and `header.size` is the size of the whole packet.
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != ERROR_SUCCESS.0 as i32 {
            continue;
        }
        let gdi = wide_to_string(&source.viewGdiDeviceName);
        if gdi.is_empty() {
            continue;
        }

        let mut entry = MonitorEntry::default();

        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        target.header.adapterId = path.targetInfo.adapterId;
        target.header.id = path.targetInfo.id;
        // SAFETY: as above.
        if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } == ERROR_SUCCESS.0 as i32 {
            entry.friendly_name = wide_to_string(&target.monitorFriendlyDeviceName)
                .trim()
                .to_owned();
        }

        let rate = path.targetInfo.refreshRate;
        if rate.Denominator != 0 {
            entry.refresh_rate = f64::from(rate.Numerator) / f64::from(rate.Denominator);
        }

        map.insert(device_key(&gdi), entry);
    }
    map
}

fn query_paths() -> Option<Vec<DISPLAYCONFIG_PATH_INFO>> {
    // The display set can change between the size query and the data query; retry a few times.
    for _ in 0..5 {
        let (mut path_count, mut mode_count) = (0u32, 0u32);
        // SAFETY: both out pointers are valid for the call.
        if unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        } != ERROR_SUCCESS
        {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: the arrays hold `path_count` / `mode_count` elements, as the counts passed in say; the
        // function writes at most that many and updates the counts.
        let result = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if result != ERROR_SUCCESS {
            return None;
        }
        paths.truncate(path_count as usize);
        return Some(paths);
    }
    None
}
