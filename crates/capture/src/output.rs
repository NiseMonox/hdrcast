//! One display output as reported by `<exe> list-outputs --json` (design.md section 2.1).

use serde::{Deserialize, Serialize, Serializer};

use crate::numfmt::{pad_right, trimmed};

/// The JSON field names are the ones the C# sender wrote, so saved settings and scripts keep working.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OutputInfo {
    /// Global index (adapter order, then output order); what `capture --output` takes.
    pub index: u32,
    pub adapter_index: u32,
    pub output_index_on_adapter: u32,
    pub adapter: String,
    /// GDI device name, e.g. `\\.\DISPLAY1`.
    pub device_name: String,
    /// Monitor friendly name from the CCD API (may be empty).
    pub friendly_name: String,
    pub width: i32,
    pub height: i32,
    pub x: i32,
    pub y: i32,
    /// DXGI_MODE_ROTATION: 0 unspecified, 1 identity, 2 = 90 degrees, 3 = 180, 4 = 270.
    pub rotation: i32,
    #[serde(serialize_with = "whole_as_integer_f64")]
    pub refresh_rate: f64,
    pub attached_to_desktop: bool,
    /// DXGI_COLOR_SPACE_TYPE numeric value (12 = RGB_FULL_G2084_NONE_P2020 = HDR on); -1 = unknown.
    pub color_space: i32,
    pub hdr: bool,
    pub bits_per_color: u32,
    #[serde(serialize_with = "whole_as_integer_f32")]
    pub min_luminance: f32,
    #[serde(serialize_with = "whole_as_integer_f32")]
    pub max_luminance: f32,
    #[serde(serialize_with = "whole_as_integer_f32")]
    pub max_full_frame_luminance: f32,
}

// The C# sender wrote whole numbers without a fraction ("refreshRate":60); keep the text the same.
fn whole_as_integer_f64<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f64(*value)
    }
}

fn whole_as_integer_f32<S: Serializer>(value: &f32, serializer: S) -> Result<S::Ok, S::Error> {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        serializer.serialize_i64(*value as i64)
    } else {
        serializer.serialize_f32(*value)
    }
}

impl OutputInfo {
    /// DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.
    pub const COLOR_SPACE_HDR10: i32 = 12;

    pub fn rotation_name(&self) -> &'static str {
        match self.rotation {
            1 => "0",
            2 => "90",
            3 => "180",
            4 => "270",
            _ => "?",
        }
    }

    /// True when the desktop image is rotated by 90 or 270 degrees (the duplicated texture is W/H swapped).
    pub fn is_rotated90(&self) -> bool {
        self.rotation == 2 || self.rotation == 4
    }

    /// One-line label for combo boxes: `0: Dell AW3225QF (\\.\DISPLAY1) 3840x2160@240 HDR`.
    pub fn label(&self) -> String {
        let mut s = format!(
            "{}: {} ({}) {}x{}",
            self.index,
            if self.friendly_name.is_empty() {
                "Display"
            } else {
                &self.friendly_name
            },
            self.device_name,
            self.width,
            self.height
        );
        if self.refresh_rate > 0.0 {
            s.push('@');
            s.push_str(&trimmed(self.refresh_rate, 1));
        }
        s.push_str(if self.hdr { " HDR" } else { " SDR" });
        if self.hdr && self.max_luminance > 0.0 {
            s.push(' ');
            s.push_str(&trimmed(f64::from(self.max_luminance), 0));
            s.push_str("nit");
        }
        s
    }

    /// The JSON array `list-outputs --json` prints.
    pub fn to_json(outputs: &[OutputInfo]) -> String {
        serde_json::to_string(outputs).expect("output descriptions always serialise")
    }

    pub fn from_json(json: &str) -> Result<Vec<OutputInfo>, serde_json::Error> {
        Ok(serde_json::from_str::<Option<Vec<OutputInfo>>>(json)?.unwrap_or_default())
    }

    /// Human-readable table for `list-outputs` without `--json`.
    pub fn to_table(outputs: &[OutputInfo]) -> String {
        let mut s = String::from(
            "idx  device         name                      size        pos          hz     hdr  colorspace  bits  maxnit  maxffl  minnit  adapter\n",
        );
        for o in outputs {
            s.push_str(&pad_right(&o.index.to_string(), 5));
            s.push_str(&pad(&o.device_name, 15));
            s.push_str(&pad(&o.friendly_name, 26));
            let rotated = if o.is_rotated90() {
                format!(" r{}", o.rotation_name())
            } else {
                String::new()
            };
            s.push_str(&pad(&format!("{}x{}{}", o.width, o.height, rotated), 12));
            s.push_str(&pad(&format!("{},{}", o.x, o.y), 13));
            s.push_str(&pad(&trimmed(o.refresh_rate, 2), 7));
            s.push_str(&pad(if o.hdr { "on" } else { "off" }, 5));
            s.push_str(&pad(&o.color_space.to_string(), 12));
            s.push_str(&pad(&o.bits_per_color.to_string(), 6));
            s.push_str(&pad(&trimmed(f64::from(o.max_luminance), 1), 8));
            s.push_str(&pad(&trimmed(f64::from(o.max_full_frame_luminance), 1), 8));
            s.push_str(&pad(&trimmed(f64::from(o.min_luminance), 4), 8));
            s.push_str(&o.adapter);
            s.push('\n');
        }
        s
    }
}

/// A table cell: padded to `width`, or cut to leave one space before the next column.
fn pad(s: &str, width: usize) -> String {
    if s.chars().count() >= width {
        let mut cut: String = s.chars().take(width - 1).collect();
        cut.push(' ');
        return cut;
    }
    pad_right(s, width)
}
