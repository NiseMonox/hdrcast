//! Asks ffmpeg which encoders it has, then actually encodes a second of video with the exact option string
//! the sender would use. That catches unsupported hardware, wrong driver versions and option-name changes
//! between ffmpeg builds before a stream is started (design.md §2.5).

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use hdrcast_core::LineProcess;
use serde::Serialize;

use super::encoder_profiles::{self, EncoderSettings, VideoEncoder};
use super::ffmpeg_command;
use crate::text::starts_with_ignore_ascii_case;
use crate::Platform;

/// How long `ffmpeg -encoders` may take.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long one probe encode may take (a first NVENC / QSV session can take seconds to come up).
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the output of a killed ffmpeg is waited for.
const DRAIN_AFTER_KILL: Duration = Duration::from_secs(3);

/// Result of probing one encoder with the real option string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderProbeResult {
    pub encoder: VideoEncoder,
    /// ffmpeg's `-encoders` names it.
    pub listed: bool,
    /// The probe encode succeeded (the encoder exists, the GPU supports it and the options parse).
    pub works: bool,
    pub message: String,
}

/// What a finished (or killed) ffmpeg run left behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegRun {
    /// stdout and stderr, line by line in the order they arrived.
    pub output: String,
    /// -1 when ffmpeg had to be killed after the timeout (the output then ends with "(timed out after … ms)").
    pub exit_code: i32,
}

/// Runs ffmpeg and returns its stdout+stderr; `None` when it could not be started (ffmpeg missing or not
/// executable). A run that outlasts `timeout` is killed together with everything it started.
pub fn run_ffmpeg<I, S>(ffmpeg_exe: &Path, args: I, timeout: Duration) -> Option<FfmpegRun>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let output = Arc::new(Mutex::new(String::new()));
    let collect = |output: &Arc<Mutex<String>>| {
        let output = Arc::clone(output);
        move |line: &str| {
            let mut text = lock(&output);
            text.push_str(line);
            text.push('\n');
        }
    };
    let (exited, exit) = mpsc::channel();

    let mut process = LineProcess::new(ffmpeg_exe.as_os_str(), args);
    process
        .on_stdout_line(collect(&output))
        .on_stderr_line(collect(&output))
        .on_exited(move |code| {
            let _ = exited.send(code);
        });
    process.start().ok()?;

    // The exit is announced only after both pipes were read to their end. Waiting for the process alone is
    // not enough: the tail of the output could still be missing, and a truncated -encoders listing would
    // look like "this encoder is not in the build".
    let exit_code = match exit.recv_timeout(timeout) {
        Ok(code) => code,
        Err(RecvTimeoutError::Disconnected) => process.exit_code().unwrap_or(-1),
        Err(RecvTimeoutError::Timeout) => {
            process.stop(); // the whole process tree
            let _ = exit.recv_timeout(DRAIN_AFTER_KILL);
            lock(&output).push_str(&format!("(timed out after {} ms)\n", timeout.as_millis()));
            -1
        }
    };
    let output = lock(&output).clone();
    Some(FfmpegRun { output, exit_code })
}

fn lock(text: &Mutex<String>) -> MutexGuard<'_, String> {
    text.lock().unwrap_or_else(|e| e.into_inner())
}

/// Encoder names ffmpeg reports in `-encoders`, in lower case (empty when ffmpeg could not be run).
pub fn list_encoders(ffmpeg_exe: &Path) -> HashSet<String> {
    run_ffmpeg(ffmpeg_exe, ["-hide_banner", "-encoders"], LIST_TIMEOUT)
        .map(|run| parse_encoder_list(&run.output))
        .unwrap_or_default()
}

/// The video encoder names in the output of `ffmpeg -encoders`, in lower case.
pub fn parse_encoder_list(output: &str) -> HashSet<String> {
    let mut found = HashSet::new();
    for line in output.split('\n') {
        // " V....D hevc_nvenc           NVIDIA NVENC hevc encoder (codec hevc)"
        let t = line.trim_end();
        let mut chars = t.char_indices();
        if chars.nth(1).map(|(_, c)| c) != Some('V') {
            continue;
        }
        // the name starts after the six flag characters; a line shorter than nine characters has none
        let Some((name_at, _)) = chars.nth(5) else {
            continue;
        };
        if chars.next().is_none() {
            continue;
        }
        if let Some(name) = t[name_at..].trim().split(' ').find(|part| !part.is_empty()) {
            found.insert(name.to_ascii_lowercase());
        }
    }
    found
}

/// Probes every encoder `platform` offers, best first; `progress` is called with each result. Setting `cancel`
/// stops before the next encoder is tried.
pub fn probe_all(
    ffmpeg_exe: &Path,
    hdr: bool,
    platform: Platform,
    mut progress: impl FnMut(&EncoderProbeResult),
    cancel: &AtomicBool,
) -> Vec<EncoderProbeResult> {
    let mut results = Vec::new();
    let listed = list_encoders(ffmpeg_exe);
    for info in encoder_profiles::for_platform(platform) {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut result = EncoderProbeResult {
            encoder: info.id,
            listed: listed.contains(&info.ffmpeg_name.to_ascii_lowercase()),
            works: false,
            message: String::new(),
        };
        if !result.listed {
            result.message = "not in this ffmpeg build".to_string();
        } else {
            let settings = EncoderSettings {
                encoder: info.id,
                hdr: hdr && info.supports_10bit,
                ..EncoderSettings::default()
            };
            match run_ffmpeg(
                ffmpeg_exe,
                ffmpeg_command::build_probe(&settings),
                PROBE_TIMEOUT,
            ) {
                None => result.message = "ffmpeg could not be started".to_string(),
                Some(run) if run.exit_code == 0 => {
                    result.works = true;
                    result.message = "ok".to_string();
                }
                Some(run) => result.message = first_error(&run.output),
            }
        }
        progress(&result);
        results.push(result);
    }
    results
}

/// The most useful line of a failed run. ffmpeg's last line is usually the generic "Nothing was written into
/// output file…"; the real reason ("Could not open encoder", "Invalid value…") comes earlier, so those are
/// preferred.
pub fn first_error(output: &str) -> String {
    const INTERESTING: [&str; 14] = [
        "could not open encoder",
        "error initializing",
        "invalid",
        "not supported",
        "no capable devices",
        "cannot load",
        "failed to",
        "unknown encoder",
        "no such",
        "unsupported",
        "out of memory",
        "driver does not",
        "requires",
        "error setting",
    ];
    let lines = || {
        output
            .split('\n')
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    if let Some(line) = lines().find(|line| {
        let lower = line.to_lowercase();
        INTERESTING.iter().any(|needle| lower.contains(needle))
    }) {
        return cut(line);
    }
    lines()
        .rev()
        .find(|line| !starts_with_ignore_ascii_case(line, "frame="))
        .map_or_else(|| "failed".to_string(), cut)
}

/// At most 200 characters: the message goes into one row of the UI.
fn cut(line: &str) -> String {
    line.chars().take(200).collect()
}

/// The first working encoder in `platform`'s order of preference.
pub fn pick_best(results: &[EncoderProbeResult], platform: Platform) -> Option<VideoEncoder> {
    encoder_profiles::auto_order(platform)
        .find(|&encoder| results.iter().any(|r| r.works && r.encoder == encoder))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: &str = "Encoders:\r\n V..... = Video\r\n A..... = Audio\r\n S..... = Subtitle\r\n .F.... = Frame-level multithreading\r\n\
        \x20..S... = Slice-level multithreading\r\n ...X.. = Codec is experimental\r\n ....B. = Supports draw_horiz_band\r\n\
        \x20.....D = Supports direct rendering method 1\r\n ------\r\n\
        \x20V....D a64multi             Multicolor charset for Commodore 64 (codec a64_multi)\r\n\
        \x20V..... libsvtav1            SVT-AV1(Scalable Video Technology for AV1) encoder (codec av1)\r\n\
        \x20V....D libx265              libx265 H.265 / HEVC (codec hevc)\r\n\
        \x20V....D hevc_nvenc           NVIDIA NVENC hevc encoder (codec hevc)\r\n\
        \x20V..... hevc_qsv             HEVC (Intel Quick Sync Video acceleration) (codec hevc)\r\n\
        \x20A....D aac                  AAC (Advanced Audio Coding)\r\n\
        \x20S..... ass                  ASS (Advanced SubStation Alpha) subtitle\r\n\
        \x20V....D x\r\n\
        \x20V....D \r\n";

    #[test]
    fn only_video_encoders_are_taken_from_the_listing() {
        let found = parse_encoder_list(LISTING);
        for name in ["a64multi", "libsvtav1", "libx265", "hevc_nvenc", "hevc_qsv"] {
            assert!(found.contains(name), "{name} missing from {found:?}");
        }
        assert!(!found.contains("aac") && !found.contains("ass"));
        assert!(found.contains("x"), "a one-letter name is still a name");
        // The legend's " V..... = Video" reads as an encoder called "=", as it did in the C# apps. Harmless.
        assert_eq!(found.len(), 7, "{found:?}");
        assert!(parse_encoder_list("").is_empty());
        assert!(parse_encoder_list("ffmpeg: command not found").is_empty());
    }

    #[test]
    fn names_are_looked_up_whatever_their_case() {
        let found = parse_encoder_list(
            " V....D HEVC_NVENC           NVIDIA NVENC hevc encoder (codec hevc)\n",
        );
        assert!(found.contains("hevc_nvenc"));
    }

    /// ffmpeg's last line for a failed encode is the generic "Nothing was written into output file"; the
    /// reason is earlier in the output, and that is what the UI should show.
    #[test]
    fn the_real_reason_is_preferred_over_the_generic_last_line() {
        let output = "[hevc_nvenc @ 0x1] Cannot load nvEncodeAPI64.dll\n\
                      [vost#0:0/hevc_nvenc @ 0x2] Could not open encoder before EOF\n\
                      [out#0/null @ 0x3] Nothing was written into output file, because at least one of its streams received no packets.\n";
        let message = first_error(output);
        assert!(message.contains("nvEncodeAPI64"), "{message}");
        assert!(!message.contains("Nothing was written"), "{message}");
    }

    #[test]
    fn falls_back_to_the_last_line_when_nothing_matches() {
        assert_eq!(
            first_error("something odd happened\n\n"),
            "something odd happened"
        );
        assert_eq!(first_error("\n\n"), "failed");
        // the statistics line is never the reason
        assert_eq!(
            first_error("it went wrong\r\nframe=    0 fps=0.0 q=0.0 size=       0KiB\r\n"),
            "it went wrong"
        );
        assert_eq!(first_error("FRAME=1\n"), "failed");
    }

    #[test]
    fn a_long_reason_is_cut_to_one_row() {
        let long = format!("Invalid {}", "é".repeat(400));
        assert_eq!(first_error(&long).chars().count(), 200);
    }

    #[test]
    fn the_best_working_encoder_follows_the_platform_order() {
        let result = |encoder, works| EncoderProbeResult {
            encoder,
            listed: true,
            works,
            message: String::new(),
        };
        let results = [
            result(VideoEncoder::HevcNvenc, false),
            result(VideoEncoder::LibX265, true),
            result(VideoEncoder::HevcQsv, true),
            result(VideoEncoder::HevcVideoToolbox, true),
        ];
        assert_eq!(
            pick_best(&results, Platform::Windows),
            Some(VideoEncoder::HevcQsv)
        );
        assert_eq!(
            pick_best(&results, Platform::MacOs),
            Some(VideoEncoder::HevcVideoToolbox)
        );
        assert_eq!(pick_best(&results[..1], Platform::Windows), None);
        assert_eq!(pick_best(&[], Platform::MacOs), None);
    }

    #[test]
    fn an_ffmpeg_that_is_not_there_is_reported_not_guessed() {
        let missing = Path::new("hdrcast-no-such-ffmpeg-here");
        assert_eq!(
            run_ffmpeg(missing, ["-version"], Duration::from_secs(5)),
            None
        );
        assert!(list_encoders(missing).is_empty());
        let mut seen = Vec::new();
        let results = probe_all(
            missing,
            true,
            Platform::Windows,
            |r| seen.push(r.clone()),
            &AtomicBool::new(false),
        );
        assert_eq!(results.len(), 4);
        assert_eq!(results, seen);
        assert!(results
            .iter()
            .all(|r| !r.listed && !r.works && r.message == "not in this ffmpeg build"));
        assert_eq!(
            probe_all(
                missing,
                true,
                Platform::MacOs,
                |_| {},
                &AtomicBool::new(false)
            )
            .len(),
            2
        );
        // already cancelled: nothing is tried
        assert!(probe_all(
            missing,
            true,
            Platform::Windows,
            |_| {},
            &AtomicBool::new(true)
        )
        .is_empty());
    }
}
