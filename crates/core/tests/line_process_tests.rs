//! LineProcess and ProcessJob against real child processes. (The C# apps had no direct tests for LineProcess;
//! its behaviour was only exercised through ffmpeg and mpv.)
//!
//! The child is this test binary itself, started again with a `hdrcast-child:<mode>` argument: `child_main`
//! below then plays the part of ffmpeg / mpv instead of returning at once. No shell and no helper program, so
//! the same children run on Windows, macOS and Linux.

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hdrcast_core::{LineProcess, ProcessJob};

const CHILD_ARG: &str = "hdrcast-child:";
/// The test harness prints its own header on the child's stdout first; the child's real output starts after
/// this line.
const MARK: &str = "<<child>>";
const LONG: Duration = Duration::from_secs(20);

fn child_args(mode: &str) -> Vec<String> {
    vec!["child_main".to_string(), "--exact".to_string(), "--nocapture".to_string(), format!("{CHILD_ARG}{mode}")]
}

fn child(mode: &str) -> LineProcess {
    LineProcess::new(std::env::current_exe().unwrap(), child_args(mode))
}

/// The child half of the tests in this file (and an empty test when run normally).
#[test]
fn child_main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(at) = args.iter().position(|a| a.starts_with(CHILD_ARG)) else { return };
    let mode = args[at][CHILD_ARG.len()..].to_string();
    let code = run_child(&mode, &args[at + 1..]);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}

fn run_child(mode: &str, rest: &[String]) -> i32 {
    let mut parts = mode.split(':');
    let kind = parts.next().unwrap_or("");
    let mut number = || parts.next().and_then(|p| p.parse::<i32>().ok()).unwrap_or(0);
    println!("{MARK}");
    match kind {
        "lines" => {
            let (count, code) = (number(), number());
            for i in 0..count {
                println!("out {i}");
                eprintln!("err {i}");
            }
            code
        }
        "status" => {
            print!("frame=1\rframe=2\rframe=3\n\r\nlast line without ending");
            0
        }
        "binary" => {
            let mut out = std::io::stdout().lock();
            out.write_all(&binary_payload()).unwrap();
            out.flush().unwrap();
            eprintln!("binary done");
            0
        }
        "echo" => {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).unwrap_or(0) > 0 {
                println!("echo {}", line.trim_end());
                line.clear();
            }
            println!("eof");
            7
        }
        "nostdin" => {
            let mut all = Vec::new();
            let n = std::io::stdin().read_to_end(&mut all).unwrap_or(0);
            println!("stdin ended after {n} bytes");
            0
        }
        "args" => {
            for a in rest {
                println!("{a}");
            }
            0
        }
        "cwd" => {
            println!("{}", std::env::current_dir().unwrap().display());
            0
        }
        "sleep" => sleep_forever(),
        "tree" => {
            // a grandchild that inherits this process's stdout and stderr, like a helper ffmpeg might start
            let mut grandchild = std::process::Command::new(std::env::current_exe().unwrap())
                .args(child_args("sleep"))
                .stdin(std::process::Stdio::null())
                .spawn()
                .unwrap();
            println!("grandchild {}", grandchild.id());
            let _ = std::io::stdout().flush();
            // minutes: the test kills both long before
            let _ = grandchild.wait();
            0
        }
        "host" => {
            // an app with a job and a child in it, about to be killed the hard way
            let job = Arc::new(ProcessJob::new());
            let mut inner = child("sleep");
            let (tx, rx) = mpsc::channel();
            inner.on_stdout_line(move |line| {
                if line == MARK {
                    let _ = tx.send(());
                }
            });
            inner.job(Some(job.clone()));
            inner.start().unwrap();
            // Only once the child is up and has nothing more to print: one that is still writing when its
            // reader dies would end on the broken pipe, job or no job.
            rx.recv_timeout(LONG).unwrap();
            println!("grandchild {}", inner.id().unwrap());
            sleep_forever()
        }
        other => {
            eprintln!("unknown child mode {other}");
            99
        }
    }
}

fn sleep_forever() -> i32 {
    let _ = std::io::stdout().flush();
    std::thread::sleep(Duration::from_secs(300));
    0
}

/// Every byte value, including the ones a text reader would trip over (0x00, CR, LF, invalid UTF-8).
fn binary_payload() -> Vec<u8> {
    (0..64).flat_map(|_| 0..=255u8).collect()
}

/// A started child with everything it said.
struct Run {
    process: LineProcess,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
    /// The exit code, with the number of stdout and stderr lines that had arrived when it was announced.
    exited: Receiver<(i32, usize, usize)>,
}

fn start(mode: &str, configure: impl FnOnce(&mut LineProcess)) -> Run {
    let mut process = child(mode);
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let (tx, exited) = mpsc::channel();
    let (out, err) = (stdout.clone(), stderr.clone());
    process.on_stdout_line(move |line| out.lock().unwrap().push(line.to_string()));
    process.on_stderr_line(move |line| err.lock().unwrap().push(line.to_string()));
    let (out, err) = (stdout.clone(), stderr.clone());
    process.on_exited(move |code| {
        let _ = tx.send((code, out.lock().unwrap().len(), err.lock().unwrap().len()));
    });
    configure(&mut process);
    process.start().unwrap();
    Run { process, stdout, stderr, exited }
}

impl Run {
    /// The child's own stdout lines (after the harness header).
    fn output(&self) -> Vec<String> {
        let lines = self.stdout.lock().unwrap();
        match lines.iter().position(|l| l == MARK) {
            Some(at) => lines[at + 1..].to_vec(),
            None => Vec::new(),
        }
    }

    fn wait_for_output(&self, what: &str, found: impl Fn(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + LONG;
        loop {
            let output = self.output();
            if found(&output) {
                return output;
            }
            assert!(Instant::now() < deadline, "the child never printed {what}; stdout so far: {output:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_until_running(&self) {
        let deadline = Instant::now() + LONG;
        while !self.stdout.lock().unwrap().iter().any(|l| l == MARK) {
            assert!(Instant::now() < deadline, "the child never started");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn exit(&self) -> (i32, usize, usize) {
        self.exited.recv_timeout(LONG).expect("the exit was never announced")
    }
}

#[test]
fn lines_of_both_pipes_arrive_in_order_and_the_exit_comes_after_the_last_line() {
    let run = start("lines:3000:3", |_| {});
    let (code, out_seen, err_seen) = run.exit();
    assert_eq!(code, 3);
    let expected_out: Vec<String> = (0..3000).map(|i| format!("out {i}")).collect();
    let expected_err: Vec<String> = (0..3000).map(|i| format!("err {i}")).collect();
    assert_eq!(run.output(), expected_out);
    assert_eq!(*run.stderr.lock().unwrap(), expected_err);
    // nothing arrived after the exit was announced
    assert_eq!(out_seen, run.stdout.lock().unwrap().len());
    assert_eq!(err_seen, 3000);

    assert_eq!(run.process.exit_code(), Some(3));
    assert!(run.process.has_started());
    assert!(run.process.has_exited());
    assert!(run.process.wait_for_exit(Duration::ZERO));
    assert!(run.process.id().is_some());
    run.process.stop(); // a no-op now
    assert!(run.exited.try_recv().is_err(), "the exit is announced once");
}

#[test]
fn the_exit_waits_for_line_callbacks_that_are_still_busy() {
    // The child is long gone while its last lines are still being handled (a log window that is slow to
    // append). Whoever hears "exited" must already have heard everything the child said: the runners decide
    // from the last lines why it ended.
    let mut process = child("lines:150:0");
    let seen = Arc::new(Mutex::new(0usize));
    let (tx, rx) = mpsc::channel();
    let counter = seen.clone();
    process.on_stderr_line(move |_| {
        std::thread::sleep(Duration::from_millis(2));
        *counter.lock().unwrap() += 1;
    });
    let counter = seen.clone();
    process.on_exited(move |code| tx.send((code, *counter.lock().unwrap())).unwrap());
    process.start().unwrap();
    // the process itself ends long before its 150 lines are handled
    assert!(process.wait_for_exit(LONG));
    assert_eq!(rx.recv_timeout(LONG).unwrap(), (0, 150));
}

#[test]
fn carriage_returns_end_lines_and_a_last_line_needs_no_ending() {
    let run = start("status", |_| {});
    assert_eq!(run.exit().0, 0);
    assert_eq!(run.output(), ["frame=1", "frame=2", "frame=3", "", "last line without ending"]);
}

#[test]
fn raw_stdout_is_handed_over_byte_for_byte_while_stderr_still_comes_as_lines() {
    let run = start("binary", |p| {
        p.read_stdout_lines(false);
    });
    let mut pipe = run.process.take_stdout().expect("the raw stdout");
    assert!(run.process.take_stdout().is_none(), "handed over once");
    let mut all = Vec::new();
    pipe.read_to_end(&mut all).unwrap();
    let mark = format!("{MARK}\n").into_bytes();
    let at = all.windows(mark.len()).position(|w| w == mark).expect("the marker") + mark.len();
    assert!(all[at..] == binary_payload()[..], "{} bytes after the marker", all.len() - at);

    assert_eq!(run.exit().0, 0);
    assert_eq!(*run.stderr.lock().unwrap(), ["binary done"]);
    assert!(run.stdout.lock().unwrap().is_empty(), "no line callback for a raw stdout");
}

#[test]
fn stdin_can_be_fed_and_closing_it_is_end_of_input() {
    let run = start("echo", |p| {
        p.redirect_stdin(true);
    });
    let mut stdin = run.process.take_stdin().expect("the child's stdin");
    assert!(run.process.take_stdin().is_none(), "handed over once");
    stdin.write_all("one\n二\n".as_bytes()).unwrap();
    drop(stdin);
    assert_eq!(run.exit().0, 7);
    assert_eq!(run.output(), ["echo one", "echo 二", "eof"]);
}

#[test]
fn without_redirection_the_child_has_no_stdin_to_wait_on() {
    let run = start("nostdin", |_| {});
    assert!(run.process.take_stdin().is_none());
    assert_eq!(run.exit().0, 0);
    assert_eq!(run.output(), ["stdin ended after 0 bytes"]);
}

#[test]
fn arguments_arrive_exactly_whatever_they_contain() {
    // spaces, quotes, a trailing backslash, an ampersand, CJK: what a path or an SRT URL can hold
    let tricky = ["直播 \"引号\"", r"C:\dir with space\", "srt://h:1?a=1&b=2", "a\\\"b", "", "末尾"];
    let mut args = child_args("args");
    args.extend(tricky.iter().map(|s| s.to_string()));
    let mut process = LineProcess::new(std::env::current_exe().unwrap(), args);
    let lines = Arc::new(Mutex::new(Vec::new()));
    let (tx, rx) = mpsc::channel();
    let sink = lines.clone();
    process.on_stdout_line(move |l| sink.lock().unwrap().push(l.to_string()));
    process.on_exited(move |code| tx.send(code).unwrap());
    process.start().unwrap();
    assert_eq!(rx.recv_timeout(LONG).unwrap(), 0);
    let lines = lines.lock().unwrap();
    let at = lines.iter().position(|l| l == MARK).unwrap();
    assert_eq!(lines[at + 1..], tricky);
}

#[test]
fn the_working_directory_is_the_childs_current_directory() {
    let dir = std::env::temp_dir();
    let run = start("cwd", |p| {
        p.working_directory(&dir);
    });
    assert_eq!(run.exit().0, 0);
    let reported = std::path::PathBuf::from(&run.output()[0]);
    assert_eq!(reported.canonicalize().unwrap(), dir.canonicalize().unwrap());
}

#[test]
fn stop_kills_a_running_child_and_the_exit_is_still_announced() {
    let run = start("sleep", |_| {});
    run.wait_until_running();
    assert!(!run.process.has_exited());
    assert_eq!(run.process.exit_code(), None);
    assert!(!run.process.wait_for_exit(Duration::from_millis(50)));

    let before = Instant::now();
    run.process.stop();
    assert!(before.elapsed() < Duration::from_secs(3), "stop() took {:?}", before.elapsed());
    assert!(run.process.has_exited());
    let (code, ..) = run.exit();
    assert_ne!(code, 0, "a killed process does not report success");
    assert_eq!(run.process.exit_code(), Some(code));
    run.process.stop(); // again: nothing left to do
}

#[test]
fn stop_takes_the_grandchildren_along() {
    let run = start("tree", |_| {});
    run.wait_for_output("its grandchild", |lines| lines.iter().any(|l| l.starts_with("grandchild ")));
    // The grandchild holds the stdout and stderr pipes it inherited. The exit is announced only when those
    // reach end of file, i.e. when the grandchild is gone too: killing just the child would leave this
    // waiting for the grandchild's five-minute sleep.
    run.process.stop();
    let (code, ..) = run.exited.recv_timeout(Duration::from_secs(10)).expect("the grandchild survived stop()");
    assert_ne!(code, 0);
}

#[test]
fn dropping_the_process_kills_it() {
    let run = start("sleep", |_| {});
    run.wait_until_running();
    let Run { process, exited, .. } = run;
    drop(process);
    assert_ne!(exited.recv_timeout(LONG).expect("the child outlived its LineProcess").0, 0);
}

#[test]
fn the_exit_callback_may_drop_the_process() {
    // What the runners do: one child's exit tears down the whole pipeline, including the child that exited.
    let slot: Arc<Mutex<Option<LineProcess>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel();
    let mut process = child("echo");
    process.redirect_stdin(true);
    let in_callback = slot.clone();
    process.on_exited(move |code| {
        let me = in_callback.lock().unwrap().take();
        let had_it = me.is_some();
        drop(me);
        tx.send((code, had_it)).unwrap();
    });
    process.start().unwrap();
    let stdin = process.take_stdin().unwrap();
    *slot.lock().unwrap() = Some(process);
    drop(stdin); // now the child may exit
    assert_eq!(rx.recv_timeout(LONG).expect("the callback deadlocked"), (7, true));
}

#[test]
fn a_panicking_line_callback_does_not_stall_the_child() {
    let mut process = child("lines:2000:0");
    let (tx, rx) = mpsc::channel();
    // resume_unwind: a panic without the message on the test log
    process.on_stdout_line(|_| std::panic::resume_unwind(Box::new("callback failed")));
    process.on_exited(move |code| tx.send(code).unwrap());
    process.start().unwrap();
    // the pipe is still emptied, so the child gets to write everything and exit by itself
    assert_eq!(rx.recv_timeout(LONG).expect("the child blocked on a pipe nobody reads"), 0);
}

#[test]
fn a_process_without_callbacks_still_runs_to_its_end() {
    let mut process = child("lines:2000:5");
    process.start().unwrap();
    assert!(process.wait_for_exit(LONG), "the child blocked on a pipe nobody reads");
    assert_eq!(process.exit_code(), Some(5));
}

#[test]
fn a_program_that_does_not_exist_is_an_error_and_leaves_nothing_behind() {
    let mut process = LineProcess::new("hdrcast-no-such-program-anywhere", ["x"]);
    assert!(!process.has_started());
    assert!(process.wait_for_exit(Duration::ZERO), "nothing to wait for");
    assert!(process.start().is_err());
    assert!(!process.has_started());
    assert!(!process.has_exited());
    assert_eq!(process.id(), None);
    assert_eq!(process.exit_code(), None);
    process.stop();
    assert_eq!(process.start().unwrap_err().to_string(), "already started");
}

#[test]
fn a_started_process_cannot_be_started_again() {
    let mut process = child("lines:1:0");
    process.start().unwrap();
    assert_eq!(process.start().unwrap_err().to_string(), "already started");
    assert!(process.wait_for_exit(LONG));
}

#[test]
fn the_logged_command_line_is_quoted_and_carries_no_secrets() {
    let process = LineProcess::new(
        r"C:\Program Files\HdrCast\tools\ffmpeg.exe",
        ["-f", "mpegts", "srt://127.0.0.1:40000?mode=listener&passphrase=a77MWmBi3_603E3kH2abfOI5O-dIU5dn&pbkeylen=32"],
    );
    assert_eq!(
        process.command_line(),
        "\"C:\\Program Files\\HdrCast\\tools\\ffmpeg.exe\" -f mpegts \"srt://127.0.0.1:40000?mode=listener&passphrase=a77MWmBi3_603E3kH2abfOI5O-dIU5dn&pbkeylen=32\""
    );
    let safe = process.safe_command_line();
    assert!(!safe.contains("a77MWmBi3"));
    assert!(safe.contains("passphrase=***&pbkeylen=32"));
    assert!(!format!("{process:?}").contains("a77MWmBi3"), "Debug output goes to logs too");
    assert_eq!(process.args().len(), 3);
    assert_eq!(LineProcess::new("mpv", Vec::<String>::new()).command_line(), "mpv");
}

// ---- ProcessJob ---------------------------------------------------------------------------------

#[test]
fn a_job_kills_its_children_when_it_goes_away() {
    let job = ProcessJob::new();
    assert!(job.is_available());
    let mut sleeper = std::process::Command::new(std::env::current_exe().unwrap())
        .args(child_args("sleep"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(job.assign(&sleeper));
    assert!(sleeper.try_wait().unwrap().is_none(), "assigning does not disturb the child");

    drop(job);
    let deadline = Instant::now() + LONG;
    while sleeper.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "the child outlived its job");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_released_child_is_left_alone_where_the_job_is_only_a_list() {
    // On Windows the kernel tracks the job's members and release() does nothing; elsewhere a released (reaped)
    // child's id must not be signalled later, because it may belong to another process by then.
    if cfg!(windows) {
        return;
    }
    let job = ProcessJob::new();
    let mut sleeper = std::process::Command::new(std::env::current_exe().unwrap())
        .args(child_args("sleep"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(job.assign(&sleeper));
    job.release(sleeper.id());
    drop(job);
    std::thread::sleep(Duration::from_millis(200));
    assert!(sleeper.try_wait().unwrap().is_none(), "a released child was killed");
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
}

/// design.md §2.7: the app is killed from Task Manager (or crashes) while ffmpeg / mpv run. They are in its job,
/// the job's last handle goes with the app, and Windows kills them. This is the whole point of ProcessJob, and
/// it also shows that LineProcess really puts its child into the job it was given.
#[cfg(windows)]
#[test]
fn children_die_with_an_app_that_is_killed_the_hard_way() {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

    let mut app = std::process::Command::new(std::env::current_exe().unwrap())
        .args(child_args("host"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // read the app's stdout until it names its child
    let mut stdout = std::io::BufReader::new(app.stdout.take().unwrap());
    let pid: u32 = loop {
        let mut line = String::new();
        assert!(std::io::BufRead::read_line(&mut stdout, &mut line).unwrap() > 0, "the app exited early");
        if let Some(pid) = line.trim().strip_prefix("grandchild ") {
            break pid.parse().unwrap();
        }
    };
    // SAFETY: plain Win32 calls on a handle we open, check and close here.
    unsafe {
        let child = OpenProcess(PROCESS_SYNCHRONIZE, false, pid).expect("the app's child is running");
        assert_ne!(WaitForSingleObject(child, 100), WAIT_OBJECT_0, "the child ended by itself");

        app.kill().unwrap(); // TerminateProcess: no destructor, no cleanup of any kind runs in the app
        app.wait().unwrap();

        let gone = WaitForSingleObject(child, 10_000) == WAIT_OBJECT_0;
        let _ = CloseHandle(child);
        assert!(gone, "the child outlived the app that was killed");
    }
}
