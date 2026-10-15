//! Command line of the capture engine (`<exe> capture ...`, design.md section 2.1) plus the debug switches.
//! Pure logic: no platform dependencies.

use std::fmt;

use crate::app_audio;
use crate::numfmt;

/// Requested output pixel format of the capture engine (`--format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CaptureFormat {
    /// p010 when the desktop is HDR, nv12 otherwise.
    #[default]
    Auto,
    /// HDR10: PQ / BT.2020 / limited range 4:2:0 10 bit (p010le).
    P010,
    /// SDR: BT.709 / limited range 4:2:0 8 bit (nv12).
    Nv12,
}

impl CaptureFormat {
    /// The value `--format` takes: `auto`, `p010` or `nv12`.
    pub fn name(self) -> &'static str {
        match self {
            CaptureFormat::P010 => "p010",
            CaptureFormat::Nv12 => "nv12",
            CaptureFormat::Auto => "auto",
        }
    }
}

impl fmt::Display for CaptureFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Returned by [`CaptureArgs::parse`] for malformed command lines (exit code 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureArgsError(pub String);

impl fmt::Display for CaptureArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CaptureArgsError {}

fn err<T>(message: impl Into<String>) -> Result<T, CaptureArgsError> {
    Err(CaptureArgsError(message.into()))
}

/// Parsed command line of the capture engine. Build one with struct-update syntax
/// (`CaptureArgs { output_index: 1, fps: 60, ..Default::default() }`) and turn it into the child's
/// arguments with [`to_argv`](Self::to_argv).
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureArgs {
    /// Global output index (order of `enumerate_outputs`); -1 = not given.
    pub output_index: i32,
    pub fps: u32,
    pub format: CaptureFormat,
    /// Requested output width, or 0 for the display's native size.
    pub width: u32,
    /// Requested output height, or 0 for the display's native size.
    pub height: u32,
    pub sdr_white: f32,
    pub cursor: bool,
    pub video_pipe: String,
    pub audio_pipe: String,
    pub audio: bool,
    /// "default" or an endpoint id.
    pub audio_device: String,
    /// Executable whose audio (with everything it started) is left out, e.g. "Discord.exe"; "" = none.
    pub audio_exclude: String,
    pub pipe_timeout_seconds: u32,
    /// Process id to watch; the engine exits when that process is gone. 0 = none.
    pub parent_pid: u32,
    /// Debug: write raw frames to this file instead of the pipe.
    pub dump_path: Option<String>,
    /// Debug: seconds to run in dump mode (0 = until stopped).
    pub dump_seconds: u32,
    /// Debug: run the shader on a synthetic gradient and print values.
    pub self_test: bool,
}

impl Default for CaptureArgs {
    fn default() -> Self {
        Self {
            output_index: -1,
            fps: Self::DEFAULT_FPS,
            format: CaptureFormat::Auto,
            width: 0,
            height: 0,
            sdr_white: Self::DEFAULT_SDR_WHITE,
            cursor: true,
            video_pipe: Self::DEFAULT_VIDEO_PIPE.to_owned(),
            audio_pipe: Self::DEFAULT_AUDIO_PIPE.to_owned(),
            audio: true,
            audio_device: "default".to_owned(),
            audio_exclude: String::new(),
            pipe_timeout_seconds: Self::DEFAULT_PIPE_TIMEOUT_SECONDS,
            parent_pid: 0,
            dump_path: None,
            dump_seconds: 0,
            self_test: false,
        }
    }
}

impl CaptureArgs {
    pub const DEFAULT_FPS: u32 = 60;
    pub const DEFAULT_SDR_WHITE: f32 = 80.0;
    pub const DEFAULT_VIDEO_PIPE: &'static str = "hdrcast-video";
    pub const DEFAULT_AUDIO_PIPE: &'static str = "hdrcast-audio";
    pub const DEFAULT_PIPE_TIMEOUT_SECONDS: u32 = 30;

    /// Defaults for capturing output `output_index`.
    pub fn for_output(output_index: u32) -> Self {
        Self {
            output_index: output_index as i32,
            ..Self::default()
        }
    }

    pub fn has_explicit_size(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// The help text for the `capture` verb; `exe` is the program name to show.
    pub fn usage(exe: &str) -> String {
        format!(
            "usage: {exe} capture --output <idx> [options]\n\
             \x20 --output <idx>            DXGI output index (see list-outputs)\n\
             \x20 --fps <n>                 constant output frame rate (default 60)\n\
             \x20 --format auto|p010|nv12   auto = p010 when the desktop is HDR, else nv12\n\
             \x20 --size WxH                output size (bilinear scaling in the shader); default native\n\
             \x20 --sdr-white <nits>        luminance of scRGB 1.0 (default 80)\n\
             \x20 --cursor on|off           draw the mouse cursor (default on)\n\
             \x20 --video-pipe <name>       named pipe for video (default hdrcast-video)\n\
             \x20 --audio-pipe <name>       named pipe for audio (default hdrcast-audio)\n\
             \x20 --audio on|off            capture desktop audio (default on)\n\
             \x20 --audio-device default|<endpoint id>\n\
             \x20 --audio-exclude <app.exe> leave this program's audio out (e.g. the voice chat)\n\
             \x20 --pipe-timeout <s>        seconds to wait for the pipe client (default 30)\n\
             \x20 --parent-pid <pid>        exit when this process disappears\n\
             \x20 --dump <file> [--seconds <n>]  write raw frames to a file instead of the pipe\n\
             \x20 --selftest                run the conversion shader on a synthetic gradient\n"
        )
    }

    /// Parses the arguments that follow the `capture` verb.
    pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<CaptureArgs, CaptureArgsError> {
        let mut a = CaptureArgs::default();
        let mut it = args.iter().map(AsRef::as_ref);
        while let Some(opt) = it.next() {
            let mut next = || {
                it.next()
                    .ok_or_else(|| CaptureArgsError(format!("missing value after {opt}")))
            };
            match opt.to_lowercase().as_str() {
                "--output" => a.output_index = parse_int(opt, next()?, 0, 63)?,
                "--fps" => a.fps = parse_int(opt, next()?, 1, 480)? as u32,
                "--format" => a.format = parse_format(next()?)?,
                "--size" => (a.width, a.height) = parse_size(next()?)?,
                "--sdr-white" => a.sdr_white = parse_float(opt, next()?, 1.0, 10000.0)?,
                "--cursor" => a.cursor = parse_on_off(opt, next()?)?,
                "--video-pipe" => a.video_pipe = parse_pipe_name(opt, next()?)?,
                "--audio-pipe" => a.audio_pipe = parse_pipe_name(opt, next()?)?,
                "--audio" => a.audio = parse_on_off(opt, next()?)?,
                "--audio-device" => {
                    let v = next()?;
                    if v.is_empty() {
                        return err("--audio-device needs a value");
                    }
                    a.audio_device = v.to_owned();
                }
                "--audio-exclude" => {
                    let v = next()?;
                    a.audio_exclude = app_audio::normalize_exe_name(v);
                    if a.audio_exclude.is_empty() {
                        return err(format!(
                            "--audio-exclude: expected a program name like Discord.exe, got {v}"
                        ));
                    }
                }
                "--pipe-timeout" => {
                    a.pipe_timeout_seconds = parse_int(opt, next()?, 1, 3600)? as u32
                }
                "--parent-pid" => a.parent_pid = parse_int(opt, next()?, 1, i32::MAX)? as u32,
                "--dump" => {
                    let v = next()?;
                    if v.is_empty() {
                        return err("--dump needs a file path");
                    }
                    a.dump_path = Some(v.to_owned());
                }
                "--seconds" => a.dump_seconds = parse_int(opt, next()?, 1, 86400)? as u32,
                "--selftest" => a.self_test = true,
                _ => return err(format!("unknown option: {opt}")),
            }
        }
        if !a.self_test && a.output_index < 0 {
            return err("--output <idx> is required");
        }
        if a.dump_seconds > 0 && a.dump_path.is_none() {
            return err("--seconds requires --dump <file>");
        }
        Ok(a)
    }

    /// Builds the argument vector (after the `capture` verb) for this configuration - used by the app to
    /// launch the engine.
    pub fn to_argv(&self) -> Vec<String> {
        let mut list: Vec<String> = Vec::new();
        let mut push = |option: &str, value: String| {
            list.push(option.to_owned());
            list.push(value);
        };
        // A self-test has no output; "--output -1" would not parse back.
        if self.output_index >= 0 {
            push("--output", self.output_index.to_string());
        }
        push("--fps", self.fps.to_string());
        push("--format", self.format.name().to_owned());
        if self.has_explicit_size() {
            push("--size", format!("{}x{}", self.width, self.height));
        }
        push("--sdr-white", numfmt::trimmed(f64::from(self.sdr_white), 3));
        push("--cursor", on_off(self.cursor));
        push("--video-pipe", self.video_pipe.clone());
        push("--audio", on_off(self.audio));
        if self.audio {
            push("--audio-pipe", self.audio_pipe.clone());
            if self.audio_device != "default" {
                push("--audio-device", self.audio_device.clone());
            }
            if !self.audio_exclude.is_empty() {
                push("--audio-exclude", self.audio_exclude.clone());
            }
        }
        if self.pipe_timeout_seconds != Self::DEFAULT_PIPE_TIMEOUT_SECONDS {
            push("--pipe-timeout", self.pipe_timeout_seconds.to_string());
        }
        if self.parent_pid > 0 {
            push("--parent-pid", self.parent_pid.to_string());
        }
        if let Some(path) = &self.dump_path {
            push("--dump", path.clone());
            if self.dump_seconds > 0 {
                push("--seconds", self.dump_seconds.to_string());
            }
        }
        if self.self_test {
            list.push("--selftest".to_owned());
        }
        list
    }
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_owned()
}

fn parse_int(opt: &str, value: &str, min: i32, max: i32) -> Result<i32, CaptureArgsError> {
    let Ok(n) = value.trim().parse::<i32>() else {
        return err(format!("{opt}: not an integer: {value}"));
    };
    if n < min || n > max {
        return err(format!("{opt}: {n} is out of range [{min}..{max}]"));
    }
    Ok(n)
}

fn parse_float(opt: &str, value: &str, min: f32, max: f32) -> Result<f32, CaptureArgsError> {
    let Ok(f) = value.trim().parse::<f32>() else {
        return err(format!("{opt}: not a number: {value}"));
    };
    // NaN is out of range too.
    if !(min..=max).contains(&f) {
        return err(format!("{opt}: {value} is out of range [{min}..{max}]"));
    }
    Ok(f)
}

fn parse_on_off(opt: &str, value: &str) -> Result<bool, CaptureArgsError> {
    match value.to_lowercase().as_str() {
        "on" | "1" | "true" | "yes" => Ok(true),
        "off" | "0" | "false" | "no" => Ok(false),
        _ => err(format!("{opt}: expected on|off, got {value}")),
    }
}

fn parse_format(value: &str) -> Result<CaptureFormat, CaptureArgsError> {
    match value.to_lowercase().as_str() {
        "auto" => Ok(CaptureFormat::Auto),
        "p010" => Ok(CaptureFormat::P010),
        "nv12" => Ok(CaptureFormat::Nv12),
        _ => err(format!("--format: expected auto|p010|nv12, got {value}")),
    }
}

fn parse_pipe_name(opt: &str, value: &str) -> Result<String, CaptureArgsError> {
    if value.is_empty() || value.contains('\\') || value.contains('/') {
        return err(format!(
            "{opt}: give only the pipe name (without \\\\.\\pipe\\), got {value}"
        ));
    }
    Ok(value.to_owned())
}

/// Parses "WxH"; both must be even and at least 2 (the shader writes 2x2 blocks).
pub fn parse_size(value: &str) -> Result<(u32, u32), CaptureArgsError> {
    let bad = || CaptureArgsError(format!("--size: expected WxH, got {value}"));
    let x = value.find(['x', 'X']).ok_or_else(bad)?;
    if x == 0 || x == value.len() - 1 {
        return Err(bad());
    }
    let width: i32 = value[..x].trim().parse().map_err(|_| bad())?;
    let height: i32 = value[x + 1..].trim().parse().map_err(|_| bad())?;
    if !(2..=16384).contains(&width) || !(2..=16384).contains(&height) {
        return err(format!("--size: {value} is out of range"));
    }
    if width & 1 != 0 || height & 1 != 0 {
        return err(format!(
            "--size: width and height must be even (4:2:0 output), got {value}"
        ));
    }
    Ok((width as u32, height as u32))
}

/// Quotes an argv for display / for a single command-line string (Windows `CommandLineToArgvW` rules).
pub fn quote_argv<S: AsRef<str>>(argv: &[S]) -> String {
    let mut out = String::new();
    for s in argv.iter().map(AsRef::as_ref) {
        if !out.is_empty() {
            out.push(' ');
        }
        if !s.is_empty() && !s.contains([' ', '\t', '"']) {
            out.push_str(s);
            continue;
        }
        out.push('"');
        let mut backslashes = 0usize;
        for c in s.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' => {
                    out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    out.push('"');
                    backslashes = 0;
                }
                _ => {
                    out.extend(std::iter::repeat_n('\\', backslashes));
                    backslashes = 0;
                    out.push(c);
                }
            }
        }
        out.extend(std::iter::repeat_n('\\', backslashes * 2));
        out.push('"');
    }
    out
}
