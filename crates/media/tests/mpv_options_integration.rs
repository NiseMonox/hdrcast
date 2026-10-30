//! Checks that every option the receiver passes to mpv actually exists (port of
//! tests/MpvOptionsIntegrationTests.cs). A typo here does not fail a unit test: it makes mpv exit immediately
//! on the viewer's machine with "Option not found", which is a bad way to find out.
//!
//! Option names are shared across platforms, so the macOS argument list is checked against whatever mpv is at
//! hand too. That proves its names exist and its plain values parse; it does not replace a run on a real Mac.
//! The exception is a video backend's own options: only an mpv built with Direct3D 11 (Windows) lists
//! `--d3d11-…`, so elsewhere those are left out with a note.
//!
//! Skipped, with a note, when mpv is missing (see tests/common/mod.rs).

mod common;

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

use common::{note, run, tool_or_skip};
use hdrcast_media::receiver::mpv_command;
use hdrcast_media::receiver::{DisplayMode, HwDecMode, ReceiverFrontend, ReceiverSettings};
use hdrcast_media::sender::encoder_probe;
use hdrcast_media::Platform;

const PLATFORMS: [Platform; 2] = [Platform::Windows, Platform::MacOs];

/// Options of the Mac's argument lists that only an mpv built for macOS has: an mpv without any `--macos-…` option
/// (Windows') leaves them unchecked, with a note; an mpv on a Mac checks them.
const MAC_ONLY: [&str; 2] = ["macos-menu-shortcuts", "native-fs"];

/// mpv's own list: option name -> the rest of its line (type, choices, default).
fn known_options(mpv: &Path) -> HashMap<String, String> {
    let (output, _) = run(mpv, ["--list-options"]);
    let mut options = HashMap::new();
    for line in output.lines() {
        let Some(option) = line.trim_start().strip_prefix("--") else {
            continue;
        };
        let (name, description) = option.split_once([' ', '\t', '=']).unwrap_or((option, ""));
        if !name.is_empty() {
            options.insert(name.trim().to_string(), description.trim().to_string());
        }
    }
    options
}

fn variants() -> Vec<ReceiverSettings> {
    let mut all = Vec::new();
    for display in [
        DisplayMode::Auto,
        DisplayMode::HdrPassthrough,
        DisplayMode::SdrToneMap,
    ] {
        for hw_dec in [
            HwDecMode::AutoSafe,
            HwDecMode::D3d11va,
            HwDecMode::D3d11vaCopy,
            HwDecMode::No,
        ] {
            for fullscreen in [false, true] {
                for frontend in [ReceiverFrontend::MpvDirect, ReceiverFrontend::FfmpegPipe] {
                    let s = ReceiverSettings {
                        latency_ms: 1500,
                        expected_bitrate_mbps: 60,
                        display_mode: display,
                        hw_dec,
                        fullscreen,
                        fullscreen_screen: 1,
                        frontend,
                        target_peak_nits: if display == DisplayMode::SdrToneMap {
                            400
                        } else {
                            0
                        },
                        ..ReceiverSettings::default()
                    };
                    all.push(s.normalized());
                }
            }
        }
    }
    all
}

/// `--name=value` / `--name` -> (name, value).
fn split_option(arg: &str) -> Option<(&str, Option<&str>)> {
    let option = arg.strip_prefix("--")?;
    Some(match option.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (option, None),
    })
}

#[test]
fn every_generated_mpv_option_exists() {
    let Some(mpv) = tool_or_skip("mpv", "every_generated_mpv_option_exists") else {
        return;
    };
    let known = known_options(&mpv);
    assert!(known.len() > 100, "could not read mpv's option list");
    // control mode's --d3d11-sync-interval (Windows) is unknown to an mpv without Direct3D 11, a Mac's say
    let has_d3d11 = known.keys().any(|name| name.starts_with("d3d11-"));
    let mut not_here = BTreeSet::new();

    for platform in PLATFORMS {
        let mut unknown = BTreeSet::new();
        let mut names = BTreeSet::new();
        // the Mac's own options, when this mpv is not a Mac's
        let mut mac_only = BTreeSet::new();
        for s in variants() {
            let plan = mpv_command::build(
                &s,
                40123,
                "0123456789abcdef",
                "hdrcast-ipc",
                Path::new("mpv"),
                Some(Path::new("ffmpeg")),
                platform,
            )
            .unwrap();
            // and control mode's list (docs/design.md §11.3)
            let low_latency = mpv_command::low_latency_args(&s, "hdrcast-ipc", platform);
            for arg in plan.mpv_args.iter().chain(&low_latency) {
                let Some((name, _)) = split_option(arg) else {
                    continue;
                };
                if !has_d3d11 && name.starts_with("d3d11-") {
                    not_here.insert(name.to_string());
                    continue;
                }
                names.insert(name.to_string());
                // mpv accepts --no-<flag> for every boolean option.
                let bare = name.strip_prefix("no-").unwrap_or(name);
                if !known.contains_key(name) && !known.contains_key(bare) {
                    if MAC_ONLY.contains(&name) && !known.keys().any(|known| known.starts_with("macos-")) {
                        mac_only.insert(name.to_string());
                    } else {
                        unknown.insert(name.to_string());
                    }
                }
            }
        }
        assert!(
            unknown.is_empty(),
            "{platform:?}: mpv does not know these options: {unknown:?}"
        );
        note(&format!(
            "mpv knows all {} option names of the {platform:?} argument lists{}",
            names.len() - mac_only.len(),
            if mac_only.is_empty() { String::new() } else { format!(" ({mac_only:?}: not an mpv for macOS, unchecked)") }
        ));
    }
    if !not_here.is_empty() {
        note(&format!(
            "this mpv has no Direct3D 11, so {not_here:?} went unchecked (an mpv on Windows checks them)"
        ));
    }
    if !not_here.is_empty() {
        note(&format!(
            "this mpv has no Direct3D 11, so {not_here:?} went unchecked (an mpv on Windows checks them)"
        ));
    }
}

/// `--list-options` also prints the choices of an option: every value we can generate for such an option must
/// be one of them (a tone-mapping algorithm mpv dropped, a hint value it renamed).
#[test]
fn every_generated_choice_is_one_mpv_offers() {
    let Some(mpv) = tool_or_skip("mpv", "every_generated_choice_is_one_mpv_offers") else {
        return;
    };
    let known = known_options(&mpv);
    // "Choices: auto no yes (default: auto)", "Choices: auto (or an integer) (10 to 10000) (default: auto)"
    let choices = |name: &str| -> Option<Vec<String>> {
        let list = known.get(name)?.strip_prefix("Choices:")?;
        Some(
            list.split('(')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_string)
                .collect(),
        )
    };

    let mut checked = 0;
    let mut all_settings = variants();
    for algorithm in ReceiverSettings::TONE_MAPPING_ALGORITHMS {
        all_settings.push(ReceiverSettings {
            display_mode: DisplayMode::SdrToneMap,
            tone_mapping: algorithm.to_string(),
            ..ReceiverSettings::default()
        });
    }
    for platform in PLATFORMS {
        for s in &all_settings {
            let plan = mpv_command::build(
                s,
                40123,
                "0123456789abcdef",
                "hdrcast-ipc",
                Path::new("mpv"),
                Some(Path::new("ffmpeg")),
                platform,
            )
            .unwrap();
            let low_latency = mpv_command::low_latency_args(s, "hdrcast-ipc", platform);
            for arg in plan.mpv_args.iter().chain(&low_latency) {
                let Some((name, Some(value))) = split_option(arg) else {
                    continue;
                };
                let Some(offered) = choices(name) else {
                    continue;
                };
                // "(or an integer)" options take their numbers as well
                let numeric = known[name].contains("or an integer") && value.parse::<i64>().is_ok();
                assert!(
                    numeric || offered.iter().any(|choice| choice == value),
                    "{platform:?}: --{name}={value} is not one of {offered:?}"
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 100,
        "mpv's choice lists were not read ({checked} values checked)"
    );
    let tone_mapping = choices("tone-mapping").expect("mpv lists the tone-mapping algorithms");
    for algorithm in ReceiverSettings::TONE_MAPPING_ALGORITHMS {
        assert!(
            tone_mapping.iter().any(|choice| choice == algorithm),
            "mpv has no tone-mapping={algorithm}: {tone_mapping:?}"
        );
    }
    note(&format!("mpv offers every generated choice value ({checked} checked, all {} tone-mapping algorithms)", ReceiverSettings::TONE_MAPPING_ALGORITHMS.len()));
}

#[test]
fn mpv_accepts_the_generated_command_line() {
    let Some(mpv) = tool_or_skip("mpv", "mpv_accepts_the_generated_command_line") else {
        return;
    };

    // Point mpv at a file that does not exist: option parsing still happens first, so a bad option or a bad
    // value is reported, while the missing file is an expected, harmless failure.
    let cases = [
        ReceiverSettings {
            latency_ms: 1500,
            expected_bitrate_mbps: 60,
            display_mode: DisplayMode::HdrPassthrough,
            ..ReceiverSettings::default()
        },
        ReceiverSettings {
            display_mode: DisplayMode::SdrToneMap,
            tone_mapping: "spline".to_string(),
            target_peak_nits: 400,
            fullscreen: true,
            fullscreen_screen: 1,
            buffer_seconds: 2.5,
            frontend: ReceiverFrontend::FfmpegPipe,
            ..ReceiverSettings::default()
        },
    ];
    for platform in PLATFORMS {
        for s in &cases {
            let s = s.clone().normalized();
            let plan = mpv_command::build(
                &s,
                40123,
                "0123456789abcdef",
                "hdrcast-ipc",
                &mpv,
                Some(Path::new("ffmpeg")),
                platform,
            )
            .unwrap();

            let mut args: Vec<String> = plan
                .mpv_args
                .iter()
                .filter(|a| {
                    !a.starts_with("srt://") // do not actually connect
                        && a.as_str() != "-" // ... or wait for stdin
                        && !a.starts_with("--input-ipc-server=")
                        && !a.starts_with("--vo=") // no window during a test run
                        && !a.starts_with("--gpu-api=")
                        && !a.starts_with("--hwdec=")
                        // the Mac's own options: only an mpv on a Mac has them
                        && (cfg!(target_os = "macos") || !MAC_ONLY.iter().any(|o| a.starts_with(&format!("--{o}="))))
                })
                .cloned()
                .collect();
            if cfg!(windows) && !args.iter().any(|a| a == "--media-controls=no") {
                // Another platform's list run on this Windows mpv: without the Windows media controls off, mpv 0.41
                // stalls on its way out after the missing file (docs/design.md §3.1), and the test would time out.
                args.push("--media-controls=no".to_string());
            }
            args.extend(
                [
                    "--vo=null",
                    "--ao=null",
                    "/nonexistent/hdrcast-option-check.ts",
                ]
                .map(String::from),
            );

            // run_ffmpeg runs any program with a timeout; a hung mpv must not hang the test run
            let outcome = encoder_probe::run_ffmpeg(&mpv, &args, Duration::from_secs(30))
                .expect("mpv starts");
            let output = &outcome.output;
            assert_ne!(
                outcome.exit_code, -1,
                "mpv did not exit by itself:\n{output}"
            );
            for complaint in [
                "Option not found",
                "is not a valid value",
                "Error parsing option",
                "Unknown option",
                "Error parsing commandline",
                "must be >=",
                "must be <=",
            ] {
                assert!(
                    !output.contains(complaint),
                    "{platform:?}: mpv complained ({complaint}):\n{output}"
                );
            }
            // it got as far as trying to open the file: the command line was accepted
            assert!(
                output.contains("hdrcast-option-check.ts"),
                "{platform:?}: mpv never tried the file:\n{output}"
            );
        }
        note(&format!("mpv parsed 2 {platform:?} command lines (without --vo/--gpu-api/--hwdec, which need a display) and went on to open the file"));
    }
}

/// The decoder names are plain strings to mpv's option parser, so a typo in one is only reported when a video
/// is decoded. What can be checked without a display: mpv's own list of decoders for the machine it runs on.
#[test]
fn the_hardware_decoders_of_this_platform_exist_in_mpv() {
    let Some(mpv) = tool_or_skip("mpv", "the_hardware_decoders_of_this_platform_exist_in_mpv")
    else {
        return;
    };
    let (list, _) = run(&mpv, ["--hwdec=help"]);
    let mut checked = Vec::new();
    for mode in [HwDecMode::D3d11va, HwDecMode::D3d11vaCopy] {
        let value = mode.mpv_value(Platform::CURRENT);
        if value.starts_with("auto") {
            continue; // no single API is named on this platform
        }
        assert!(
            list.lines()
                .any(|line| line.trim_start().starts_with(&format!("{value} "))),
            "mpv --hwdec=help does not list {value}:\n{list}"
        );
        checked.push(value);
    }
    note(&format!("mpv --hwdec=help lists {checked:?} (the names used on {:?}; other platforms' names cannot be checked here)", Platform::CURRENT));
}
