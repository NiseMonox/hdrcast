//! The Windows backend: DXGI Desktop Duplication + a D3D11 compute shader for the picture, WASAPI loopback for
//! the sound, named pipes towards ffmpeg.
//!
//! `unsafe` here is FFI only; each block states what it relies on.

mod audio;
mod console;
mod converter;
mod cursor;
mod display_config;
mod duplicator;
mod nvenc;
mod outputs;
mod pipe;
mod process;
mod process_loopback;
mod selftest;
mod surfaces;
mod timer;
mod util;
mod video;

use std::io;

use crate::args::{CaptureArgs, EncodeArgs, EngineEncoder};
use crate::encode::VideoEncoder;
use crate::output::OutputInfo;
use crate::platform::{
    AudioDeviceInfo, AudioSink, AudioSource, Backend, FrameLayout, PipeServer, ProcessWatch,
    VideoSource,
};

pub(crate) use console::{
    install_stop_handler, local_time_of_day, prepare_cli, remove_stop_handler,
};
pub(crate) use pipe::{pipe_listening, pipe_path};
pub(crate) use timer::PreciseSleep;

pub(crate) struct NativeBackend;

impl NativeBackend {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl Backend for NativeBackend {
    fn open_video(&self, args: &CaptureArgs) -> Result<Box<dyn VideoSource>, String> {
        Ok(Box::new(video::open(args)?))
    }

    fn create_pipe(&self, name: &str, out_buffer_bytes: usize) -> io::Result<Box<dyn PipeServer>> {
        Ok(Box::new(pipe::NamedPipeServer::create(
            name,
            out_buffer_bytes,
        )?))
    }

    fn open_audio(
        &self,
        device: &str,
        exclude_app: &str,
        sink: AudioSink,
    ) -> Result<Box<dyn AudioSource>, String> {
        Ok(Box::new(audio::open(device, exclude_app, sink)?))
    }

    fn watch_process(&self, pid: u32) -> Option<Box<dyn ProcessWatch>> {
        Some(Box::new(process::ProcessHandleWatch::open(pid)?))
    }

    fn self_test(&self, sdr_white: f32) -> i32 {
        selftest::run(sdr_white)
    }

    fn open_encoder(
        &self,
        args: &EncodeArgs,
        layout: FrameLayout,
        fps: u32,
    ) -> Result<Box<dyn VideoEncoder>, String> {
        // Frames in memory: zero copy, where possible, comes from the video source (`surface_encoder`).
        match args.encoder {
            EngineEncoder::Nvenc => Ok(Box::new(nvenc::NvencEncoder::open(
                args, layout, fps, None,
            )?)),
        }
    }
}

pub(crate) fn enumerate_outputs() -> Result<Vec<OutputInfo>, String> {
    outputs::enumerate()
}

pub(crate) fn list_audio_devices() -> Vec<AudioDeviceInfo> {
    audio::list_devices()
}

pub(crate) fn list_audio_apps(device: &str) -> Vec<String> {
    audio::list_audio_apps(device)
}

#[cfg(test)]
mod tests {
    use crate::shader;
    use windows::Win32::Globalization::{WideCharToMultiByte, WC_NO_BEST_FIT_CHARS};

    /// `text` in the ANSI code page `code_page`, the way .NET's `Encoding.GetBytes` (and SharpDX's string
    /// marshalling) produced it.
    fn to_code_page(text: &str, code_page: u32) -> Vec<u8> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        // UTF-8 (65001) accepts no flags.
        let flags = if code_page == 65001 {
            0
        } else {
            WC_NO_BEST_FIT_CHARS
        };
        // SAFETY: both slices are valid; the first call only measures.
        unsafe {
            let size = WideCharToMultiByte(code_page, flags, &wide, None, None, None);
            assert!(size > 0, "code page {code_page} is not installed");
            let mut bytes = vec![0u8; size as usize];
            let written =
                WideCharToMultiByte(code_page, flags, &wide, Some(&mut bytes), None, None);
            bytes.truncate(written as usize);
            bytes
        }
    }

    /// 0.2.2 on Chinese Windows: "hdrcast_convert.hlsl(187,6): error X3000: syntax error: unexpected end of
    /// file". SharpDX's Compile(string) converted the source to the ANSI code page but passed the UTF-16
    /// length. Comments held a section sign and two em dashes, two bytes each in GBK and Big5, so the last 3
    /// bytes of the shader (its closing brace) never reached the compiler. Japanese Windows only lost the
    /// final newline. The Rust engine hands over bytes with their exact length; this keeps the source safe
    /// for anyone who ever goes through a code page again, and proves the HLSL compiles at all (both entry
    /// points) without needing a display.
    #[test]
    fn the_shader_compiles_whatever_the_ansi_code_page() {
        let source = shader::SOURCE;
        let utf16_len = source.encode_utf16().count();
        let mut failures = Vec::new();
        for code_page in [1252u32, 932, 936, 950, 949, 65001] {
            // what a string-marshalling wrapper hands the compiler: the ANSI bytes, cut to the UTF-16 length
            let mut seen = to_code_page(source, code_page);
            seen.truncate(utf16_len);
            for entry in [
                shader::ENTRY_P010,
                shader::ENTRY_NV12,
                shader::ENTRY_P010_PLANES,
                shader::ENTRY_NV12_PLANES,
            ] {
                if let Err(error) = super::converter::compile_shader(&seen, entry) {
                    failures.push(format!("code page {code_page}, {entry}: {error}"));
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    fn a_truncated_shader_is_reported_with_the_compilers_message() {
        // What 0.2.2 did on Chinese Windows: the last three bytes never reached the compiler.
        let source = shader::source_bytes();
        let error =
            super::converter::compile_shader(&source[..source.len() - 3], shader::ENTRY_NV12)
                .unwrap_err();
        assert!(error.contains("hdrcast_convert.hlsl"), "{error}");
        assert!(error.contains("error X3000"), "{error}");
    }
}
