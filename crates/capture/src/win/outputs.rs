//! Enumerates DXGI outputs in the exact order the capture engine indexes them: EnumAdapters1 -> EnumOutputs,
//! flattened. That index is what `capture --output` takes.

use std::collections::HashMap;

use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput6,
};

use super::display_config::{self, MonitorEntry};
use super::util::{error_text, wide_to_string, DpiAwareThread};
use crate::output::OutputInfo;

pub(crate) fn enumerate() -> Result<Vec<OutputInfo>, String> {
    let _dpi = DpiAwareThread::enter();
    let monitors = display_config::query_active_monitors();
    let mut result = Vec::new();
    for_each_output(|adapter, adapter_index, output, output_index_on_adapter| {
        let index = result.len() as u32;
        result.push(describe(
            output,
            &adapter_name(adapter, adapter_index),
            adapter_index,
            output_index_on_adapter,
            index,
            &monitors,
        )?);
        Ok(true)
    })?;
    Ok(result)
}

/// Calls `visit(adapter, adapter index, output, output index on the adapter)` for every output in enumeration
/// order until it returns `Ok(false)`.
pub(crate) fn for_each_output(
    mut visit: impl FnMut(&IDXGIAdapter1, u32, &IDXGIOutput, u32) -> Result<bool, String>,
) -> Result<(), String> {
    // SAFETY: plain factory creation; the returned interface is reference counted by its wrapper.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| error_text(&e))?;
    for adapter_index in 0.. {
        // SAFETY: plain COM calls on live interfaces; an error (DXGI_ERROR_NOT_FOUND) ends the enumeration.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(adapter_index) }) else {
            break;
        };
        for output_index in 0.. {
            // SAFETY: as above.
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                break;
            };
            if !visit(&adapter, adapter_index, &output, output_index)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

pub(crate) fn adapter_name(adapter: &IDXGIAdapter1, adapter_index: u32) -> String {
    // SAFETY: plain COM call returning a value.
    match unsafe { adapter.GetDesc1() } {
        Ok(desc) => wide_to_string(&desc.Description).trim().to_owned(),
        Err(_) => format!("adapter {adapter_index}"),
    }
}

pub(crate) fn describe(
    output: &IDXGIOutput,
    adapter_name: &str,
    adapter_index: u32,
    output_index_on_adapter: u32,
    global_index: u32,
    monitors: &HashMap<String, MonitorEntry>,
) -> Result<OutputInfo, String> {
    // SAFETY: plain COM call returning a value.
    let d = unsafe { output.GetDesc() }.map_err(|e| error_text(&e))?;
    let device = wide_to_string(&d.DeviceName);
    let mut info = OutputInfo {
        index: global_index,
        adapter_index,
        output_index_on_adapter,
        adapter: adapter_name.to_owned(),
        width: d.DesktopCoordinates.right - d.DesktopCoordinates.left,
        height: d.DesktopCoordinates.bottom - d.DesktopCoordinates.top,
        x: d.DesktopCoordinates.left,
        y: d.DesktopCoordinates.top,
        rotation: d.Rotation.0,
        attached_to_desktop: d.AttachedToDesktop.as_bool(),
        ..OutputInfo::default()
    };

    if let Some(entry) = monitors.get(&display_config::device_key(&device)) {
        info.friendly_name = entry.friendly_name.clone();
        info.refresh_rate = entry.refresh_rate;
    }
    info.device_name = device;

    // IDXGIOutput6 (Windows 10 1703+) carries the colour space and luminance of the current desktop mode.
    // SAFETY: plain COM calls on a live interface.
    match output
        .cast::<IDXGIOutput6>()
        .and_then(|o6| unsafe { o6.GetDesc1() })
    {
        Ok(d1) => {
            info.color_space = d1.ColorSpace.0;
            info.hdr = info.color_space == OutputInfo::COLOR_SPACE_HDR10;
            info.bits_per_color = d1.BitsPerColor;
            info.min_luminance = d1.MinLuminance;
            info.max_luminance = d1.MaxLuminance;
            info.max_full_frame_luminance = d1.MaxFullFrameLuminance;
        }
        // IDXGIOutput6 unavailable: treat the desktop as SDR
        Err(_) => info.color_space = -1,
    }
    Ok(info)
}
