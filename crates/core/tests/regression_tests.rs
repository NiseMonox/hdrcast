//! Port of the parts of tests/RegressionTests.cs that only need this crate: SecretRedactorTests and
//! ProcessJobTests. Defects found in review; each test fails again if the fix is undone.

use hdrcast_core::secret_redactor::{redact, MASK};
use hdrcast_core::ProcessJob;

// ---- SecretRedactorTests ------------------------------------------------------------------------

#[test]
fn passphrase_is_masked_in_a_url() {
    let url = "srt://relay:8890?streamid=publish:screen&passphrase=SuperSecret123&pbkeylen=32&latency=800000";
    let safe = redact(url);
    assert!(!safe.contains("SuperSecret123"));
    assert!(safe.contains(&format!("passphrase={MASK}")));
    assert!(safe.contains("latency=800000")); // everything else is still readable
    assert!(safe.contains("pbkeylen=32"));
}

#[test]
fn stream_id_password_is_masked_but_the_path_and_user_survive() {
    let url = "srt://relay:8890?streamid=publish:screen:sender:hunter2&latency=800000";
    let safe = redact(url);
    assert!(!safe.contains("hunter2"));
    assert!(safe.contains(&format!("publish:screen:sender:{MASK}")));
}

#[test]
fn standard_haivision_stream_id_secret_is_masked() {
    let url = "srt://relay:8890?streamid=#!::m=publish,r=screen,u=sender,s=hunter2";
    let safe = redact(url);
    assert!(!safe.contains("hunter2"));
    assert!(safe.contains("u=sender"));
}

#[test]
fn a_whole_command_line_is_redacted() {
    let line = "[run] \"C:\\tools\\ffmpeg.exe\" -f mpegts \"srt://h:8890?streamid=publish:screen:sender:pw123&passphrase=abcdefghij\"";
    let safe = redact(line);
    assert!(!safe.contains("pw123"));
    assert!(!safe.contains("abcdefghij"));
    assert!(safe.contains("ffmpeg.exe"));
}

#[test]
fn text_without_secrets_is_unchanged() {
    const LINE: &str = "[ffmpeg] frame= 1234 fps=59.9 bitrate=39876.5kbits/s";
    assert_eq!(redact(LINE), LINE);
    assert_eq!(redact(""), "");
}

#[test]
fn a_stream_id_without_credentials_is_left_alone() {
    const URL: &str = "srt://relay:8890?streamid=read:screen&latency=1500000";
    assert_eq!(redact(URL), URL);
}

// ---- ProcessJobTests ----------------------------------------------------------------------------

#[test]
fn job_creation_never_fails_whatever_the_host() {
    // The app creates one unconditionally; where job objects do not exist it must degrade quietly.
    // (Assigning the test process itself is not tried: on Windows the job is real, KILL_ON_JOB_CLOSE, and
    // dropping it would kill the test run. tests/line_process_tests.rs assigns real children.)
    let job = ProcessJob::new();
    if cfg!(windows) {
        assert!(job.is_available(), "a job object can be created on any supported Windows");
    }
    job.release(0); // unknown ids are ignored
    drop(job);
    let _ = ProcessJob::default();
}
