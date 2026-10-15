//! The Windows backend against the real machine: named pipes, the outputs, and whole capture sessions run
//! through the `hdrcast-capture` program the way the app runs them.
//!
//! The capture tests need an interactive desktop; they skip (with a note) when no output is attached. They
//! capture at 640x360 so a run stays small, which also exercises the scaling path of the shader.
#![cfg(windows)]

mod common;

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hdrcast_capture::platform::{self, PipeServer};
use hdrcast_capture::{
    color, exit_codes, pipe_listening, pipe_path, CaptureArgs, CaptureFormat, CaptureStats,
    OutputInfo,
};

const ENGINE: &str = env!("CARGO_BIN_EXE_hdrcast-capture");

/// A pipe name nobody else uses.
fn unique(tag: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    format!(
        "hdrcast-test-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

// ---------------------------------------------------------------- named pipes

#[test]
fn pipe_path_uses_the_windows_prefix() {
    assert_eq!(pipe_path("foo"), r"\\.\pipe\foo");
}

/// v0.2.0 on Windows: the runner polled `File.Exists(@"\\.\pipe\...")` before starting ffmpeg. Asking a pipe
/// for its attributes connects to it, so the probe took the engine's only instance: ffmpeg got
/// ERROR_PIPE_BUSY ("Error opening input: Invalid argument") on every start and nothing was ever encoded.
/// `pipe_listening` (WaitNamedPipe) only asks.
#[test]
fn probing_a_pipe_never_takes_its_only_instance() {
    let name = unique("probe");
    assert!(!pipe_listening(&name), "the pipe does not exist yet");

    let mut server: Box<dyn PipeServer> = platform::native().create_pipe(&name, 1 << 20).unwrap();
    // The app polls like this until both pipes are there, as often as it likes.
    for _ in 0..20 {
        assert!(
            pipe_listening(&name),
            "the pipe exists from the moment it is created"
        );
    }
    assert!(
        !server.wait_for_client(Duration::from_millis(50)).unwrap(),
        "probing is not connecting"
    );
    assert!(pipe_listening(&name));

    // The one instance is still free for the real client (ffmpeg).
    let path = pipe_path(&name);
    let client = std::thread::spawn(move || {
        let mut pipe =
            std::fs::File::open(path).expect("the client can still connect after the probes");
        let mut received = Vec::new();
        pipe.read_to_end(&mut received).unwrap();
        received
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !server.wait_for_client(Duration::from_millis(100)).unwrap() {
        assert!(Instant::now() < deadline, "the client never connected");
    }
    assert!(
        server.wait_for_client(Duration::ZERO).unwrap(),
        "connected stays connected"
    );
    assert!(!pipe_listening(&name), "the only instance is taken now");

    // More than the pipe's buffer in one write: it blocks until the client read it, and arrives intact.
    let payload: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    server.write_all(&payload).unwrap();
    server.write_all(b"end").unwrap();
    drop(server); // the client reads what is still in the pipe, then sees the end
    let received = client.join().unwrap();
    assert_eq!(received.len(), payload.len() + 3);
    assert!(received[..payload.len()] == payload[..]);
    assert_eq!(&received[payload.len()..], b"end");
}

#[test]
fn a_write_fails_once_the_consumer_went_away_and_a_stuck_one_can_be_cancelled() {
    let name = unique("gone");
    let mut server = platform::native().create_pipe(&name, 4096).unwrap();
    let client = std::fs::File::open(pipe_path(&name)).unwrap();
    assert!(server.wait_for_client(Duration::from_secs(5)).unwrap());
    server.write_all(b"while the client is there").unwrap();
    drop(client);
    let error = server.write_all(&[0u8; 100_000]).unwrap_err();
    assert!(
        error.raw_os_error().is_some(),
        "an OS error (broken pipe / pipe is being closed): {error}"
    );

    // A client that connects but never reads: the write blocks until somebody cancels it.
    let name = unique("stuck");
    let mut server = platform::native().create_pipe(&name, 4096).unwrap();
    let _client = std::fs::File::open(pipe_path(&name)).unwrap();
    assert!(server.wait_for_client(Duration::from_secs(5)).unwrap());
    let canceller = server.canceller();
    let started = Instant::now();
    let writer = std::thread::spawn(move || server.write_all(&vec![0u8; 8 << 20]));
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !writer.is_finished(),
        "nobody reads, so the write is still pending"
    );
    canceller.cancel();
    let result = writer.join().unwrap();
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_second_engine_cannot_take_over_a_pipe_name() {
    let name = unique("single");
    let _first = platform::native().create_pipe(&name, 4096).unwrap();
    assert!(platform::native().create_pipe(&name, 4096).is_err());
}

/// The static half of the same regression: no source in the workspace may look for a pipe by opening it or by
/// asking for its attributes. (A unix backend may `stat` its FIFO - that does not connect - by saying "FIFO"
/// on the same line.)
#[test]
fn named_pipes_are_never_probed_by_opening_them() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let probes = [
        ".exists()",
        ".try_exists()",
        "fs::metadata(",
        "symlink_metadata(",
        ".is_file()",
        "fs::exists(",
    ];
    let mut offenders = Vec::new();
    let mut pending = vec![crates.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if !matches!(name.as_str(), "target" | "tests" | "node_modules" | "gen")
                    && !name.starts_with('.')
                {
                    pending.push(path);
                }
            } else if name.ends_with(".rs") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for (number, line) in text.lines().enumerate() {
                    let code = line.trim();
                    let lower = code.to_lowercase();
                    if code.starts_with("//")
                        || !lower.replace("pipeline", "").contains("pipe")
                        || lower.contains("fifo")
                    {
                        continue;
                    }
                    if probes.iter().any(|probe| code.contains(probe)) {
                        offenders.push(format!("{}:{}: {code}", path.display(), number + 1));
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "use pipe_listening() instead:\n{}",
        offenders.join("\n")
    );
}

// ---------------------------------------------------------------- the real machine

/// One capture at a time: the tests below share the displays and the audio endpoint.
fn hardware() -> MutexGuard<'static, ()> {
    static HARDWARE: Mutex<()> = Mutex::new(());
    HARDWARE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The outputs that can be captured, or `None` (after a note) when this machine has none (no session).
fn attached_outputs() -> Option<Vec<OutputInfo>> {
    let outputs = match hdrcast_capture::enumerate_outputs() {
        Ok(outputs) => outputs,
        Err(message) => {
            eprintln!("note: cannot enumerate outputs ({message}); skipping the capture checks");
            return None;
        }
    };
    let attached: Vec<OutputInfo> = outputs
        .into_iter()
        .filter(|o| o.attached_to_desktop && o.width > 0)
        .collect();
    if attached.is_empty() {
        eprintln!("note: no display attached to the desktop; skipping the capture checks");
        return None;
    }
    Some(attached)
}

fn engine(args: &[&str]) -> (i32, String, String) {
    let (code, stdout, stderr) = common::run(Path::new(ENGINE), args);
    (code, String::from_utf8_lossy(&stdout).into_owned(), stderr)
}

/// Physical size of a display mode straight from GDI (independent of the caller's DPI awareness).
fn gdi_mode_size(device_name: &str) -> Option<(i32, i32)> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    let device: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: `device` is NUL-terminated and `mode` is a properly sized DEVMODEW.
    let ok =
        unsafe { EnumDisplaySettingsW(PCWSTR(device.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };
    ok.as_bool()
        .then_some((mode.dmPelsWidth as i32, mode.dmPelsHeight as i32))
}

/// The test process has no DPI manifest. On a display scaled to 150 % a DPI-unaware caller would be told
/// 2560x1440 for a 3840x2160 output, and the capture would be set up for the wrong size.
#[test]
fn outputs_are_listed_in_physical_pixels_whatever_the_callers_dpi_awareness() {
    let Some(outputs) = attached_outputs() else {
        return;
    };
    for (position, output) in outputs.iter().enumerate() {
        assert!(
            !output.device_name.is_empty() && !output.adapter.is_empty(),
            "{output:?}"
        );
        if position > 0 {
            assert!(
                output.index > outputs[position - 1].index,
                "indexes follow the enumeration order"
            );
        }
        let Some((mode_width, mode_height)) = gdi_mode_size(&output.device_name) else {
            continue;
        };
        let expected = if output.is_rotated90() {
            (mode_height, mode_width)
        } else {
            (mode_width, mode_height)
        };
        // GDI reports the mode in the panel's orientation on some drivers and rotated on others.
        assert!(
            (output.width, output.height) == expected
                || (output.width, output.height) == (mode_width, mode_height),
            "{} is {}x{} according to GDI, listed as {}x{}",
            output.device_name,
            mode_width,
            mode_height,
            output.width,
            output.height
        );
        assert_eq!(
            output.hdr,
            output.color_space == OutputInfo::COLOR_SPACE_HDR10
        );
    }
}

#[test]
fn list_outputs_prints_what_the_library_enumerates() {
    let _hardware = hardware();
    let Ok(expected) = hdrcast_capture::enumerate_outputs() else {
        return;
    };
    let (code, stdout, stderr) = engine(&["list-outputs", "--json"]);
    assert_eq!(code, exit_codes::OK, "{stderr}");
    assert_eq!(stdout.lines().count(), 1, "one line of JSON");
    assert_eq!(OutputInfo::from_json(&stdout).unwrap(), expected);

    let (code, table, _) = engine(&["list-outputs"]);
    assert_eq!(code, exit_codes::OK);
    assert_eq!(table, OutputInfo::to_table(&expected));

    let (code, _, stderr) = engine(&["list-outputs", "--xml"]);
    assert_eq!(code, exit_codes::BAD_ARGUMENTS);
    assert_eq!(stderr, "unknown option: --xml\n");
}

/// With `HDRCAST_REFERENCE_EXE` pointing at the C# sender (HdrCastSender.exe 0.2.4) the two engines are
/// compared directly: same outputs, same self-test numbers.
#[test]
fn the_csharp_engine_reports_the_same_outputs_and_selftest() {
    let Some(reference) = std::env::var_os("HDRCAST_REFERENCE_EXE") else {
        eprintln!(
            "note: HDRCAST_REFERENCE_EXE is not set; skipping the comparison with the C# engine"
        );
        return;
    };
    let _hardware = hardware();
    let (code, stdout, _) = common::run(Path::new(&reference), &["list-outputs", "--json"]);
    assert_eq!(code, 0);
    let theirs = OutputInfo::from_json(&String::from_utf8_lossy(&stdout)).unwrap();
    assert_eq!(hdrcast_capture::enumerate_outputs().unwrap(), theirs);

    // Compare the numbers, not the bytes: the C# program prints in the console's code page.
    let numbers = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| line.starts_with("  ") || line.starts_with("device:"))
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect()
    };
    let (code, theirs, _) = common::run(Path::new(&reference), &["capture", "--selftest"]);
    assert_eq!(code, 0);
    let (code, ours, _) = engine(&["capture", "--selftest"]);
    assert_eq!(code, 0);
    let ours = numbers(&ours);
    assert_eq!(ours, numbers(&String::from_utf8_lossy(&theirs)));
    assert_eq!(ours.len(), 9, "{ours:?}");
}

#[test]
fn the_selftest_passes_on_this_gpu() {
    let (code, stdout, stderr) = engine(&["capture", "--selftest"]);
    assert_eq!(code, exit_codes::OK, "{stdout}{stderr}");
    assert!(stdout.starts_with("device: "), "{stdout}");
    assert!(
        stdout.contains("scRGB -> HDR10 (PQ, BT.2020, limited range 10 bit), sdr-white = 80 nit")
    );
    assert!(
        stdout.contains("  chroma of grey: Cb=512 Cr=512 ok"),
        "{stdout}"
    );
    assert!(
        stdout.ends_with(
            "selftest: PASS \u{2014} the GPU shader matches the reference implementation\n"
        ),
        "{stdout}"
    );
    assert_eq!(
        stdout.matches(" ok").count(),
        7,
        "six luma rows and the chroma row:\n{stdout}"
    );
    // A different white level moves the numbers and still matches the reference.
    let (code, stdout, _) = engine(&["capture", "--selftest", "--sdr-white", "203"]);
    assert_eq!(code, exit_codes::OK, "{stdout}");
    assert!(stdout.contains("sdr-white = 203 nit"));
}

#[test]
fn bad_arguments_exit_with_2_and_explain() {
    let (code, stdout, stderr) = engine(&["capture", "--output", "0", "--turbo"]);
    assert_eq!(code, exit_codes::BAD_ARGUMENTS);
    assert_eq!(stdout, "");
    assert!(stderr.starts_with("unknown option: --turbo\nusage: hdrcast-capture.exe capture --output <idx> [options]\n"), "{stderr}");

    let (code, _, stderr) = engine(&["capture"]);
    assert_eq!(code, exit_codes::BAD_ARGUMENTS);
    assert!(
        stderr.starts_with("--output <idx> is required\n"),
        "{stderr}"
    );

    // Not an engine mode at all: the library declines, the test program prints its usage.
    assert_eq!(hdrcast_capture::cli_main(&["gui".to_owned()]), None);
    assert_eq!(hdrcast_capture::cli_main(&[]), None);
    let (code, _, stderr) = engine(&["frobnicate"]);
    assert_eq!(code, exit_codes::BAD_ARGUMENTS);
    assert!(
        stderr.starts_with("unknown command: frobnicate\n"),
        "{stderr}"
    );
}

#[test]
fn an_output_that_does_not_exist_is_a_device_error() {
    let dir = common::TempDir::new("no-output");
    let dump = dir.file("never.raw");
    let (code, stdout, stderr) = engine(&[
        "capture",
        "--output",
        "63",
        "--dump",
        dump.to_str().unwrap(),
        "--seconds",
        "1",
    ]);
    assert_eq!(code, exit_codes::DEVICE_ERROR, "{stderr}");
    assert_eq!(stdout, "");
    assert!(
        stderr.contains("[E] capture: DXGI output 63 does not exist (run list-outputs)"),
        "{stderr}"
    );
    assert!(
        !dump.exists(),
        "nothing is created for a display that is not there"
    );
}

#[test]
fn audio_endpoints_and_apps_can_be_listed() {
    let devices = hdrcast_capture::list_audio_devices();
    for device in &devices {
        assert!(
            device.id.starts_with('{'),
            "an MMDevice endpoint id: {device:?}"
        );
    }
    assert!(devices.iter().filter(|d| d.is_default).count() <= 1);
    let (code, stdout, _) = engine(&["audio-devices"]);
    assert_eq!(code, exit_codes::OK);
    assert_eq!(stdout.lines().count(), devices.len());
    for (line, device) in stdout.lines().zip(&devices) {
        assert!(
            line.starts_with(&format!("{}\t{}", device.id, device.name)),
            "{line}"
        );
        assert_eq!(line.ends_with(" (default)"), device.is_default);
    }

    let apps = hdrcast_capture::list_audio_apps("default");
    assert!(
        apps.windows(2)
            .all(|w| w[0].to_uppercase() < w[1].to_uppercase()),
        "sorted, no duplicates: {apps:?}"
    );
    // A stale endpoint id is not an error for the list (the reason is logged).
    assert!(hdrcast_capture::list_audio_apps("{0.0.0.00000000}.{no-such-endpoint}").is_empty());
}

/// The format and size the engine announced: `... -> p010 640x360 @30fps`.
fn announced_format(stderr: &str) -> (CaptureFormat, u32, u32) {
    let line = stderr
        .lines()
        .find(|l| l.contains("capture: output "))
        .unwrap_or_else(|| panic!("no start line in:\n{stderr}"));
    let mut words = line.rsplit(' ');
    let (_fps, size, format) = (
        words.next().unwrap(),
        words.next().unwrap(),
        words.next().unwrap(),
    );
    let (width, height) = hdrcast_capture::args::parse_size(size).unwrap();
    (
        if format == "p010" {
            CaptureFormat::P010
        } else {
            CaptureFormat::Nv12
        },
        width,
        height,
    )
}

/// Every sample of a raw frame is a legal limited-range code, with P010's 10 bits in the HIGH bits.
fn assert_legal_samples(frame: &[u8], format: CaptureFormat, width: u32, height: u32) {
    let uv = color::uv_plane_offset(format, width, height);
    if format == CaptureFormat::P010 {
        for (i, sample) in frame.chunks_exact(2).enumerate() {
            let value = u16::from_le_bytes([sample[0], sample[1]]);
            assert_eq!(
                value & 0x3F,
                0,
                "sample {i}: the low six bits of p010le are empty"
            );
            let code = color::from_p010_sample(value);
            let legal = if i * 2 < uv { 64..=940 } else { 64..=960 };
            assert!(
                legal.contains(&code),
                "sample {i} (uv plane at {}): {code}",
                uv / 2
            );
        }
    } else {
        for (i, &code) in frame.iter().enumerate() {
            let legal = if i < uv { 16..=235 } else { 16..=240 };
            assert!(legal.contains(&code), "sample {i}: {code}");
        }
    }
}

#[test]
fn a_dump_of_every_output_holds_whole_legal_frames_at_the_requested_rate() {
    let Some(outputs) = attached_outputs() else {
        return;
    };
    let _hardware = hardware();
    let ffmpeg = common::find_tool("ffmpeg");
    let dir = common::TempDir::new("dump");
    for output in &outputs {
        let dump = dir.file(&format!("output{}.raw", output.index));
        let index = output.index.to_string();
        let (code, stdout, stderr) = engine(&[
            "capture",
            "--output",
            &index,
            "--size",
            "640x360",
            "--fps",
            "30",
            "--dump",
            dump.to_str().unwrap(),
            "--seconds",
            "1",
        ]);
        assert_eq!(code, exit_codes::OK, "output {index}: {stderr}");

        let (format, width, height) = announced_format(&stderr);
        assert_eq!((width, height), (640, 360));
        // auto: p010 exactly when the desktop is HDR and delivers FP16
        if !output.hdr {
            assert_eq!(
                format,
                CaptureFormat::Nv12,
                "an SDR desktop is nv12: {stderr}"
            );
        }
        let frame_bytes = color::frame_bytes(format, width, height);
        let data = std::fs::read(&dump).unwrap();
        assert_eq!(
            data.len() % frame_bytes,
            0,
            "output {index}: whole frames only"
        );
        let frames = data.len() / frame_bytes;
        assert!(
            (29..=32).contains(&frames),
            "output {index}: {frames} frames in one second at 30 fps"
        );
        for frame in data.chunks_exact(frame_bytes) {
            assert_legal_samples(frame, format, width, height);
        }
        let stats: Vec<CaptureStats> = stdout.lines().filter_map(CaptureStats::try_parse).collect();
        let total = stats
            .last()
            .unwrap_or_else(|| panic!("no statistics in {stdout:?}"));
        assert_eq!(total.dropped, 0, "{total:?}");
        assert!(
            total.captured >= 1,
            "at least the current desktop image was captured: {total:?}"
        );
        assert!((28.0..=32.5).contains(&total.fps), "{total:?}");

        if let Some(ffmpeg) = &ffmpeg {
            let pix_fmt = if format == CaptureFormat::P010 {
                "p010le"
            } else {
                "nv12"
            };
            let (code, _, log) = common::run(
                ffmpeg,
                &[
                    "-hide_banner",
                    "-loglevel",
                    "info",
                    "-f",
                    "rawvideo",
                    "-pix_fmt",
                    pix_fmt,
                    "-video_size",
                    "640x360",
                    "-framerate",
                    "30",
                    "-i",
                    dump.to_str().unwrap(),
                    "-f",
                    "null",
                    "-",
                ],
            );
            assert_eq!(code, 0, "{log}");
            assert!(
                log.contains(&format!("frame={frames:5}")),
                "ffmpeg decoded another number of frames than {frames}:\n{log}"
            );
        }
    }
}

/// What a session produced, with ffmpeg's part played by the test: both pipes are opened once they listen and
/// read on two threads until the "encoder" goes away.
struct SessionResult {
    exit_code: i32,
    stats: Vec<CaptureStats>,
    stderr: String,
    video_bytes: u64,
    audio_bytes: u64,
    audio_nonzero_samples: u64,
    connect_seconds: f64,
}

fn run_session(extra: &[&str], read_for: Duration) -> SessionResult {
    run_session_while(extra, || std::thread::sleep(read_for))
}

/// Runs a session for as long as `during` takes.
fn run_session_while(extra: &[&str], during: impl FnOnce()) -> SessionResult {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (video_name, audio_name) = (unique("video"), unique("audio"));
    let parent = std::process::id().to_string();
    let mut args = vec![
        "capture",
        "--video-pipe",
        &video_name,
        "--audio-pipe",
        &audio_name,
        "--parent-pid",
        &parent,
    ];
    args.extend_from_slice(extra);
    let started = Instant::now();
    let mut child = Command::new(ENGINE)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // What the app does before it starts ffmpeg: wait until BOTH pipes are there, without touching them.
    while !(pipe_listening(&video_name) && pipe_listening(&audio_name)) {
        if child.try_wait().unwrap().is_some() || started.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "the pipes never appeared:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let connect_seconds = started.elapsed().as_secs_f64();

    let stop_reading = std::sync::Arc::new(AtomicBool::new(false));
    let reader = |name: &str, count_nonzero: bool| {
        let mut pipe = std::fs::File::open(pipe_path(name))
            .unwrap_or_else(|e| panic!("cannot open {name}: {e}"));
        let stop = std::sync::Arc::clone(&stop_reading);
        std::thread::spawn(move || {
            let mut buffer = vec![0u8; 1 << 20];
            let (mut total, mut nonzero) = (0u64, 0u64);
            while !stop.load(Ordering::SeqCst) {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        total += n as u64;
                        if count_nonzero {
                            nonzero += buffer[..n]
                                .chunks_exact(4)
                                .filter(|s| s.iter().any(|b| *b != 0))
                                .count() as u64;
                        }
                    }
                }
            }
            (total, nonzero)
        })
    };
    let video = reader(&video_name, false);
    let audio = reader(&audio_name, true);

    during();
    // ffmpeg goes away: the readers close their ends, the engine notices on its next write.
    stop_reading.store(true, Ordering::SeqCst);
    let (video_bytes, _) = video.join().unwrap();
    let (audio_bytes, audio_nonzero_samples) = audio.join().unwrap();
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    SessionResult {
        exit_code: output.status.code().unwrap_or(-1),
        stats: stdout.lines().filter_map(CaptureStats::try_parse).collect(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        video_bytes,
        audio_bytes,
        audio_nonzero_samples,
        connect_seconds,
    }
}

#[test]
fn a_session_through_both_pipes_holds_the_frame_rate_and_keeps_audio_in_step() {
    let Some(outputs) = attached_outputs() else {
        return;
    };
    let _hardware = hardware();
    let index = outputs[0].index.to_string();
    let result = run_session(
        &["--output", &index, "--size", "640x360", "--fps", "60"],
        Duration::from_millis(3300),
    );

    // The consumer went away while the engine was streaming.
    assert_eq!(
        result.exit_code,
        exit_codes::PIPE_BROKEN,
        "{}",
        result.stderr
    );
    assert!(
        result
            .stderr
            .contains("capture: the pipe consumer went away"),
        "{}",
        result.stderr
    );
    assert!(result.stderr.contains("capture: video pipe connected"));
    assert!(result.stderr.contains("audio: pipe client connected"));
    assert!(
        result.connect_seconds < 10.0,
        "both pipes were there after {:.1} s",
        result.connect_seconds
    );

    let (format, width, height) = announced_format(&result.stderr);
    let frame_bytes = color::frame_bytes(format, width, height) as f64;
    let frames = result.video_bytes as f64 / frame_bytes;
    assert!(
        (170.0..=215.0).contains(&frames),
        "{frames:.1} frames read in ~3.3 s at 60 fps"
    );

    // One statistics line per second; each full second ran at the requested rate.
    let per_second: Vec<&CaptureStats> =
        result.stats.iter().filter(|s| s.t >= 0.9).take(3).collect();
    assert_eq!(per_second.len(), 3, "{:?}", result.stats);
    for (second, stats) in per_second.iter().enumerate() {
        assert!((stats.t - (second + 1) as f64).abs() < 0.3, "{stats:?}");
        assert!((58.0..=62.0).contains(&stats.fps), "{stats:?}");
        assert_eq!(stats.dropped, 0, "{stats:?}");
        assert!(stats.gpu_ms < 50.0, "{stats:?}");
    }

    // Audio: f32le 48 kHz stereo = 384000 bytes a second, in whole frames, on the same clock as the video.
    let audio_seconds = result.audio_bytes as f64 / 384_000.0;
    assert!(
        (2.6..=3.8).contains(&audio_seconds),
        "{audio_seconds:.2} s of audio for ~3.3 s of video"
    );
    if result.stderr.contains("audio: capturing ") {
        // A real endpoint (kept awake by the silence stream) delivers continuously: nothing is patched in.
        for stats in &per_second {
            assert_eq!(stats.audio_underrun, 0, "{stats:?}\n{}", result.stderr);
        }
    } else {
        eprintln!("note: no audio endpoint on this machine; the stream carried silence");
        assert!(
            result
                .stderr
                .contains("sending silence so the stream keeps its audio track"),
            "{}",
            result.stderr
        );
        assert_eq!(result.audio_nonzero_samples, 0);
    }
}

#[test]
fn leaving_an_app_out_captures_through_process_loopback() {
    let Some(outputs) = attached_outputs() else {
        return;
    };
    let _hardware = hardware();
    let index = outputs[0].index.to_string();
    let result = run_session(
        &[
            "--output",
            &index,
            "--size",
            "640x360",
            "--fps",
            "30",
            "--audio-exclude",
            "hdrcast-no-such-app",
        ],
        Duration::from_millis(2300),
    );
    assert_eq!(
        result.exit_code,
        exit_codes::PIPE_BROKEN,
        "{}",
        result.stderr
    );
    if !result.stderr.contains("audio: capturing ") {
        eprintln!("note: no audio endpoint on this machine; nothing to leave out");
        return;
    }
    if result
        .stderr
        .contains("audio: process loopback unavailable")
    {
        // Before Windows 10 21H1: plain loopback, and the log says so.
        assert!(
            result
                .stderr
                .contains("audio: cannot leave out hdrcast-no-such-app.exe on this Windows"),
            "{}",
            result.stderr
        );
    } else {
        // The app is not running, so the engine excludes only itself and follows the process list.
        assert!(
            result.stderr.contains("audio: hdrcast-no-such-app.exe is not running; sending all desktop audio until it starts"),
            "{}",
            result.stderr
        );
        assert!(
            result.stderr.contains("audio: capturing IeeeFloat 48000 Hz 2 ch 32 bit, without hdrcast-no-such-app.exe from "),
            "{}",
            result.stderr
        );
    }
    assert!(!result.stderr.contains("[E] audio:"), "{}", result.stderr);
    let audio_seconds = result.audio_bytes as f64 / 384_000.0;
    assert!(
        (1.7..=2.8).contains(&audio_seconds),
        "{audio_seconds:.2} s of audio for ~2.3 s"
    );
    let last = result.stats.last().unwrap();
    assert_eq!(last.audio_underrun, 0, "{last:?}\n{}", result.stderr);
}

/// The engine as a library inside a host that has no DPI manifest (this test program): the capture thread
/// makes itself per-monitor aware, otherwise DuplicateOutput1 refuses and the sizes are virtualised.
#[test]
fn the_engine_runs_inside_a_host_process_without_a_dpi_manifest() {
    let Some(outputs) = attached_outputs() else {
        return;
    };
    let _hardware = hardware();
    let dir = common::TempDir::new("in-process");
    let dump = dir.file("frames.raw");
    let args = CaptureArgs {
        width: 320,
        height: 180,
        fps: 30,
        format: CaptureFormat::Nv12,
        dump_path: Some(dump.to_string_lossy().into_owned()),
        dump_seconds: 1,
        ..CaptureArgs::for_output(outputs[0].index)
    };
    let stats = std::sync::Arc::new(Mutex::new(Vec::new()));
    let sink_stats = std::sync::Arc::clone(&stats);
    let mut engine = hdrcast_capture::CaptureEngine::new(args);
    engine.set_stats_sink(move |s: &CaptureStats| sink_stats.lock().unwrap().push(*s));
    assert_eq!(engine.run(), exit_codes::OK);
    drop(engine);

    let frame_bytes = color::frame_bytes(CaptureFormat::Nv12, 320, 180);
    let data = std::fs::read(&dump).unwrap();
    assert_eq!(data.len() % frame_bytes, 0);
    assert!(
        (29..=32).contains(&(data.len() / frame_bytes)),
        "{} frames",
        data.len() / frame_bytes
    );
    for frame in data.chunks_exact(frame_bytes) {
        assert_legal_samples(frame, CaptureFormat::Nv12, 320, 180);
    }
    let stats = stats.lock().unwrap();
    assert!(
        stats
            .last()
            .is_some_and(|s| s.captured >= 1 && s.dropped == 0),
        "{stats:?}"
    );
}

/// `--parent-pid`: the app died (or was killed in the task manager), so the engine must not stay behind.
#[test]
fn the_engine_exits_when_the_process_it_watches_is_gone() {
    let Some(outputs) = attached_outputs() else {
        return;
    };
    let _hardware = hardware();
    let mut parent = Command::new("ping")
        .args(["-n", "60", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .spawn()
        .expect("a stand-in parent process");
    let dir = common::TempDir::new("parent");
    let dump = dir.file("frames.raw");
    let (index, pid) = (outputs[0].index.to_string(), parent.id().to_string());
    let child = Command::new(ENGINE)
        .args([
            "capture",
            "--output",
            &index,
            "--size",
            "320x180",
            "--fps",
            "30",
            "--dump",
            dump.to_str().unwrap(),
        ])
        .args(["--parent-pid", &pid])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    parent.kill().unwrap();
    parent.wait().unwrap();
    let killed = Instant::now();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        killed.elapsed() < Duration::from_secs(5),
        "the engine outlived its parent by {:?}",
        killed.elapsed()
    );
    assert_eq!(output.status.code(), Some(exit_codes::OK), "{stderr}");
    assert!(
        stderr.contains(&format!("capture: parent process {pid} is gone, stopping")),
        "{stderr}"
    );
    assert!(!stderr.contains("cannot watch parent process"), "{stderr}");
    // It streamed until then: about a second of frames, and a last statistics line.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last = stdout
        .lines()
        .filter_map(CaptureStats::try_parse)
        .next_back()
        .expect("statistics");
    assert!(last.captured + last.duplicated >= 20, "{last:?}");
}

/// The app to leave out starts and quits while the session runs: the engine follows it (one process snapshot
/// a second) and re-targets the process loopback, without a hole in the audio.
#[test]
fn the_engine_follows_the_app_it_leaves_out_as_it_starts_and_quits() {
    let Some(outputs) = attached_outputs() else {
        return;
    };
    let _hardware = hardware();
    // Any program will do as "the voice chat": a copy of the engine under another name, dumping a tiny capture.
    let dir = common::TempDir::new("victim");
    let victim_exe = dir.file("hdrcast-test-victim.exe");
    std::fs::copy(ENGINE, &victim_exe).unwrap();
    let victim_dump = dir.file("victim.raw");
    let index = outputs[0].index.to_string();

    let mut victim_pid = 0;
    let result = run_session_while(
        &[
            "--output",
            &index,
            "--size",
            "640x360",
            "--fps",
            "30",
            "--audio-exclude",
            "HDRCAST-TEST-VICTIM",
        ],
        || {
            std::thread::sleep(Duration::from_millis(1500));
            let mut victim = Command::new(&victim_exe)
                .args([
                    "capture", "--output", &index, "--size", "320x180", "--fps", "5", "--audio",
                    "off",
                ])
                .args(["--dump", victim_dump.to_str().unwrap()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            victim_pid = victim.id();
            std::thread::sleep(Duration::from_millis(2500)); // at least two checks of the process list
            victim.kill().unwrap();
            victim.wait().unwrap();
            std::thread::sleep(Duration::from_millis(2500));
        },
    );
    assert_eq!(
        result.exit_code,
        exit_codes::PIPE_BROKEN,
        "{}",
        result.stderr
    );
    if !result.stderr.contains("audio: capturing ")
        || result
            .stderr
            .contains("audio: process loopback unavailable")
    {
        eprintln!("note: no process loopback on this machine; nothing to follow");
        return;
    }

    let app = "HDRCAST-TEST-VICTIM.exe";
    let not_running =
        format!("audio: {app} is not running; sending all desktop audio until it starts");
    let leaving_out =
        format!("audio: leaving out {app} (pid {victim_pid} and everything it started)");
    let events: Vec<&str> = result
        .stderr
        .lines()
        .filter_map(|line| line.split_once("] ").map(|(_, message)| message))
        .filter(|message| *message == not_running || *message == leaving_out)
        .collect();
    assert_eq!(
        events,
        [
            not_running.as_str(),
            leaving_out.as_str(),
            not_running.as_str()
        ],
        "{}",
        result.stderr
    );
    assert!(
        !result.stderr.contains("[E] audio:")
            && !result.stderr.contains("for the rest of this session"),
        "{}",
        result.stderr
    );

    // The audio track kept up with the clock across both switches.
    let audio_seconds = result.audio_bytes as f64 / 384_000.0;
    assert!(
        (5.8..=7.2).contains(&audio_seconds),
        "{audio_seconds:.2} s of audio for ~6.5 s"
    );
    let last = result.stats.last().unwrap();
    assert_eq!(last.audio_underrun, 0, "{last:?}\n{}", result.stderr);
}

#[test]
fn the_engine_command_line_round_trips_through_the_program() {
    // What the app builds is what the program parses: an argument error would show up as exit code 2.
    let args = CaptureArgs {
        self_test: true,
        sdr_white: 100.0,
        ..CaptureArgs::default()
    };
    let mut argv = vec!["capture".to_owned()];
    argv.extend(args.to_argv());
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = engine(&argv);
    assert_eq!(code, exit_codes::OK, "{stderr}");
    assert!(stdout.contains("sdr-white = 100 nit"), "{stdout}");
}
