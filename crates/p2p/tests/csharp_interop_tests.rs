//! This crate against the shipped C# apps (0.2.4), the real thing: their own `ViewerSession` and `P2pHost`,
//! loaded from HdrCastReceiver.exe / HdrCastSender.exe into Windows PowerShell 5.1 (the same .NET Framework 4.8
//! they run on) and driven by a few lines of C#. A 0.2.4 viewer watches a host of this crate and a viewer of
//! this crate watches a 0.2.4 host, each through the real signaling server, a punched loopback path and SRT,
//! with ffprobe reading the HEVC at the viewer's end. Along the way: a live record that offers remote control
//! and a join that asks for it ("rc", docs/design.md §8.9) do not disturb 0.2.4, and nothing of the side
//! channel is ever sent to it.
//!
//! Windows only. Needs the unpacked 0.2.4 release: the directory named by HDRCAST_CSHARP, or the parent of
//! HDRCAST_TOOLS when the two executables are there (as in a release folder with its tools\); plus Python and
//! ffmpeg / ffprobe as for the other integration tests. Without any of them the tests print a note and pass.
//! Nothing but loopback is used: both sides get loopback addresses and no router mapping.

#![cfg(windows)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use hdrcast_core::{invite, GroupConfig, Invite};
use hdrcast_p2p::{P2pHost, P2pHostOptions, SwitchableStream, ViewerSession, ViewerSessionOptions, ViewerSessionState, ViewerState};

use common::{loopback_firewalled, wait_for, SignalServer};

/// The driver: C# 5 (what PowerShell 5.1 compiles), referencing the app's own executable.
const SCRIPT: &str = r#"
param([string]$Exe, [string]$Role, [string]$Code, [string]$HostId, [int]$EchoPort, [string]$Ffmpeg)
$ErrorActionPreference = 'Stop'
$app = [Reflection.Assembly]::LoadFrom($Exe)
Add-Type -TypeDefinition @'
using System;
using System.Reflection;
public static class AppResolver
{
    static Assembly app;
    public static void Install(Assembly a)
    {
        app = a;
        AppDomain.CurrentDomain.AssemblyResolve += delegate(object s, ResolveEventArgs e)
        {
            return new AssemblyName(e.Name).Name == app.GetName().Name ? app : null;
        };
    }
}
'@
[AppResolver]::Install($app)
$source = @'
using System;
using System.Diagnostics;
using System.Linq;
using System.Net;
using System.Threading;
using HdrCast.Group;
using HdrCast.P2p;

public static class Interop
{
    static readonly object Gate = new object();

    static void Say(string line)
    {
        lock (Gate)
        {
            Console.Out.WriteLine(line);
            Console.Out.Flush();
        }
    }

    public static void Log(string line) { Say("LOG " + line); }

    static LinkAddresses Loopback()
    {
        return new LinkAddresses { Ipv6 = IPAddress.IPv6Loopback, Ipv4Bind = IPAddress.Loopback };
    }

    static ManualResetEvent QuitOnInput()
    {
        var quit = new ManualResetEvent(false);
        var t = new Thread(delegate() { try { Console.In.ReadLine(); } catch (Exception) { } quit.Set(); });
        t.IsBackground = true;
        t.Start();
        return quit;
    }

    public static void Viewer(string code, string hostId, int echoPort)
    {
        GroupConfig g = new GroupConfig().Normalize().Adopt(InviteCodec.Decode(code), "csharp-viewer");
        var vs = new ViewerSession(new ViewerSessionOptions
        {
            Group = g, HostId = hostId, LatencyMs = 300, OverheadPercent = 25, Addresses = Loopback(),
            EchoPort = echoPort, MapRouterPort = false,
        }, Log);
        ManualResetEvent quit = QuitOnInput();
        vs.Start();
        string last = "";
        while (!quit.WaitOne(50))
        {
            string now = vs.State + "|" + vs.LocalPort + "|" + vs.SrtPassphrase + "|" + vs.Status;
            if (now != last) { Say("STATE " + now); last = now; }
        }
        vs.Stop();
        Say("DONE");
    }

    public static void Host(string code, int echoPort, string ffmpeg)
    {
        GroupConfig g = new GroupConfig().Normalize().Adopt(InviteCodec.Decode(code), "csharp-host");
        Say("HOSTID " + g.PeerId);
        const string encoderArgs =
            "-hide_banner -loglevel error -re -f lavfi -i testsrc2=size=320x180:rate=30 -t 60 -c:v libx265 -pix_fmt yuv420p10le " +
            "-x265-params keyint=30:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:log-level=error " +
            "-f mpegts -mpegts_flags +resend_headers -muxdelay 0 -muxpreload 0 pipe:1";
        Process encoder = Process.Start(new ProcessStartInfo(ffmpeg, encoderArgs)
        {
            UseShellExecute = false, RedirectStandardOutput = true, RedirectStandardError = true, CreateNoWindow = true,
        });
        encoder.ErrorDataReceived += delegate { };
        encoder.BeginErrorReadLine();
        var ts = new SwitchableStream();
        ts.Attach(encoder.StandardOutput.BaseStream);
        var host = new P2pHost(new P2pHostOptions
        {
            Group = g, FfmpegPath = ffmpeg, BitrateBps = 1000000, MaxViewers = 2, Addresses = Loopback(),
            EchoPort = echoPort, HeartbeatMs = 1000, MapRouterPort = false,
        }, ts, Log);
        ManualResetEvent quit = QuitOnInput();
        try
        {
            host.Start();
            string last = "";
            while (!quit.WaitOne(50))
            {
                string now = host.Status + "|" + string.Join(",", host.Viewers.Select(v => v.Name + ":" + v.State).ToArray());
                if (now != last) { Say("STATUS " + now); last = now; }
            }
        }
        finally
        {
            host.Stop();
            ts.Dispose();
            try { if (!encoder.HasExited) encoder.Kill(); } catch (Exception) { }
        }
        Say("DONE");
    }
}
'@
# Add-Type takes only .dll paths as references; the compiler itself takes the app's .exe.
$options = New-Object System.CodeDom.Compiler.CompilerParameters
$options.GenerateInMemory = $true
[void]$options.ReferencedAssemblies.Add($Exe)
[void]$options.ReferencedAssemblies.Add('System.dll')
[void]$options.ReferencedAssemblies.Add('System.Core.dll')
$compiled = (New-Object Microsoft.CSharp.CSharpCodeProvider).CompileAssemblyFromSource($options, $source)
if ($compiled.Errors.Count -gt 0) {
    $compiled.Errors | ForEach-Object { [Console]::Error.WriteLine($_.ToString()) }
    exit 3
}
$interop = $compiled.CompiledAssembly.GetType('Interop')
[Console]::Out.WriteLine('READY')
[Console]::Out.Flush()
if ($Role -eq 'viewer') {
    [void]$interop.GetMethod('Viewer').Invoke($null, [object[]]@($Code, $HostId, $EchoPort))
} else {
    [void]$interop.GetMethod('Host').Invoke($null, [object[]]@($Code, $EchoPort, $Ffmpeg))
}
"#;

/// The 0.2.4 release folder, when it is around.
fn csharp_release() -> Option<PathBuf> {
    let has_apps = |dir: &Path| dir.join("HdrCastSender.exe").is_file() && dir.join("HdrCastReceiver.exe").is_file();
    let named = std::env::var_os("HDRCAST_CSHARP").filter(|d| !d.is_empty()).map(PathBuf::from);
    let beside_tools = std::env::var_os("HDRCAST_TOOLS").filter(|d| !d.is_empty()).and_then(|tools| PathBuf::from(tools).parent().map(Path::to_path_buf));
    named.into_iter().chain(beside_tools).find(|dir| has_apps(dir))
}

/// One of the C# apps' own classes at work in a PowerShell process; what it prints, line by line.
struct CSharp {
    process: Child,
    stdin: Option<ChildStdin>,
    lines: Arc<Mutex<Vec<String>>>,
    errors: Arc<Mutex<String>>,
    script: PathBuf,
}

impl CSharp {
    /// `None` (after printing why) when PowerShell cannot be started or cannot compile the driver.
    fn start(test: &str, exe: &Path, role: &str, code: &str, host_id: &str, echo_port: u16, ffmpeg: &Path) -> Option<CSharp> {
        // Windows PowerShell reads a script without a byte order mark in the ANSI code page: ASCII only.
        assert!(SCRIPT.is_ascii());
        let script = std::env::temp_dir().join(format!("hdrcast-p2p-interop-{}-{role}.ps1", std::process::id()));
        std::fs::write(&script, SCRIPT).unwrap();
        let spawned = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .arg("-Exe")
            .arg(exe)
            .args(["-Role", role, "-Code", code, "-HostId", host_id, "-EchoPort", &echo_port.to_string(), "-Ffmpeg"])
            .arg(ffmpeg)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut process = match spawned {
            Ok(process) => process,
            Err(e) => {
                common::skip(test, &format!("Windows PowerShell could not be started ({e})"));
                let _ = std::fs::remove_file(&script);
                return None;
            }
        };
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(String::new()));
        let (stdout, stderr) = (process.stdout.take().unwrap(), process.stderr.take().unwrap());
        let sink = lines.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line.trim().to_string());
            }
        });
        let sink = errors.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut errors = sink.lock().unwrap();
                errors.push_str(&line);
                errors.push('\n');
            }
        });
        let stdin = process.stdin.take();
        let mut csharp = CSharp { process, stdin, lines, errors, script };
        // loading PowerShell and compiling takes a few seconds on a cold machine
        let ready = wait_for(|| csharp.has(|line| line == "READY") || !matches!(csharp.process.try_wait(), Ok(None)), 60_000);
        if !ready || !csharp.has(|line| line == "READY") {
            common::skip(test, &format!("the C# driver could not be compiled or started here:\n{}", csharp.errors.lock().unwrap()));
            return None;
        }
        Some(csharp)
    }

    fn has(&self, matches: impl Fn(&str) -> bool) -> bool {
        self.lines.lock().unwrap().iter().any(|line| matches(line))
    }

    /// The latest line that starts with `prefix`, without it.
    fn latest(&self, prefix: &str) -> Option<String> {
        self.lines.lock().unwrap().iter().rev().find_map(|line| line.strip_prefix(prefix).map(str::to_string))
    }

    fn output(&self) -> String {
        format!("{}\n--- stderr ---\n{}", self.lines.lock().unwrap().join("\n"), self.errors.lock().unwrap())
    }

    /// Asks the driver to stop its session (a line on its stdin) and waits for it to say it has.
    fn finish(&mut self) -> bool {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(b"quit\r\n");
            let _ = stdin.flush();
        }
        wait_for(|| self.has(|line| line == "DONE"), 20_000)
    }
}

impl Drop for CSharp {
    fn drop(&mut self) {
        self.stdin.take();
        if !wait_for(|| !matches!(self.process.try_wait(), Ok(None)), 3000) {
            let _ = self.process.kill();
        }
        let _ = self.process.wait();
        let _ = std::fs::remove_file(&self.script);
    }
}

type Log = Arc<Mutex<Vec<String>>>;

fn logger(log: &Log) -> impl Fn(&str) + Send + Sync + 'static {
    let log = log.clone();
    move |line| log.lock().unwrap().push(line.to_string())
}

fn dump(log: &Log) -> String {
    log.lock().unwrap().join("\n")
}

fn probe(ffprobe: &Path, local_port: u16, passphrase: &str) -> String {
    let url = format!("srt://127.0.0.1:{local_port}?passphrase={passphrase}&pbkeylen=32&latency=300000&connect_timeout=4000");
    let mut result = String::new();
    for _attempt in 0..3 {
        result = common::run(ffprobe, &["-hide_banner", "-v", "error", "-show_entries", "stream=codec_name,profile,color_transfer", "-of", "compact", &url], 15_000).0;
        if result.contains("codec_name=") {
            break;
        }
    }
    result
}

/// What both tests need; `None` after saying what is missing.
fn prerequisites(test: &str) -> Option<(PathBuf, PathBuf, PathBuf, SignalServer)> {
    let Some(release) = csharp_release() else {
        common::skip(test, "needs the unpacked C# 0.2.4 release (set HDRCAST_CSHARP to its folder, or HDRCAST_TOOLS to its tools\\)");
        return None;
    };
    let Some((ffmpeg, ffprobe)) = common::media_tools() else {
        common::skip(test, "needs ffmpeg and ffprobe with libx265 (set HDRCAST_TOOLS to their directory)");
        return None;
    };
    if !common::has_ipv6_loopback() {
        common::skip(test, "this machine has no IPv6 loopback");
        return None;
    }
    let server = SignalServer::try_start(test)?;
    Some((release, ffmpeg, ffprobe, server))
}

/// One PowerShell with the C# apps in it at a time, and one encoder.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

#[test]
fn a_0_2_4_viewer_watches_a_host_of_this_crate() {
    const TEST: &str = "a_0_2_4_viewer_watches_a_host_of_this_crate";
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some((release, ffmpeg, ffprobe, server)) = prerequisites(TEST) else { return };
    let group_invite = Invite::create("interop", Some(&server.url())).unwrap();
    let host_group = GroupConfig::default().normalize().adopt(&group_invite, "rust-host");
    let log: Log = Arc::new(Mutex::new(Vec::new()));

    let mut encoder = common::KillOnDrop(
        Command::new(&ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=30", "-t", "60"])
        .args(["-c:v", "libx265", "-pix_fmt", "yuv420p10le"])
        .args(["-x265-params", "keyint=30:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:log-level=error"])
        .args(["-f", "mpegts", "-mpegts_flags", "+resend_headers", "-muxdelay", "0", "-muxpreload", "0", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the encoder starts"),
    );
    let ts = SwitchableStream::new();
    ts.attach(Box::new(encoder.0.stdout.take().unwrap()));
    let host = P2pHost::new(
        P2pHostOptions {
            group: host_group.clone(),
            ffmpeg_path: ffmpeg.clone().into_os_string(),
            bitrate_bps: 1_000_000,
            max_viewers: 2,
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            heartbeat_ms: 1000,
            map_router_port: false, // never the real router
            // the live record carries "rc": a 0.2.4 viewer must read past it
            offer_remote_control: Some(Arc::new(|| true)),
            ..P2pHostOptions::default()
        },
        Box::new(ts.clone()),
        logger(&log),
    );
    let side_datagrams = Arc::new(Mutex::new(0usize));
    let counter = side_datagrams.clone();
    host.set_side_handler(Arc::new(move |_, _| *counter.lock().unwrap() += 1));
    host.start().unwrap();
    assert!(wait_for(|| host.status().starts_with("Live"), 8000), "{}\n{}", host.status(), dump(&log));

    let code = invite::encode(&group_invite);
    let Some(mut viewer) = CSharp::start(TEST, &release.join("HdrCastReceiver.exe"), "viewer", &code, &host_group.peer_id, server.echo_port, &ffmpeg) else {
        host.stop();
        ts.close();
        return;
    };
    println!("{TEST}: the viewer is {}", release.join("HdrCastReceiver.exe").display());
    assert!(
        wait_for(|| viewer.latest("STATE ").is_some_and(|state| state.starts_with("Connected|")), 30_000),
        "the 0.2.4 viewer did not connect:\n{}\n--- host log ---\n{}",
        viewer.output(),
        dump(&log)
    );
    let state = viewer.latest("STATE ").unwrap();
    let fields: Vec<&str> = state.splitn(4, '|').collect();
    let (local_port, passphrase, status) = (fields[1].parse::<u16>().unwrap(), fields[2], fields[3]);
    assert!(status.starts_with("Connected to rust-host (IPv6 [::1]:"), "{status}");
    assert_eq!(passphrase, host_group.keys().unwrap().srt_passphrase());

    // the picture arrives: 0.2.4's ViewerLink carries this host's SRT
    let result = probe(&ffprobe, local_port, passphrase);
    assert!(result.contains("codec_name=hevc"), "{result}\n{}\n--- host log ---\n{}", viewer.output(), dump(&log));
    assert!(result.contains("profile=Main 10"), "{result}");
    assert!(result.contains("color_transfer=smpte2084"), "{result}");

    // what this host knows of the 0.2.4 viewer: it never asked for control, so nothing is sent to it
    assert!(wait_for(|| host.viewers().iter().any(|v| v.state == ViewerState::Watching), 5000), "{:?}", host.viewers());
    let viewers = host.viewers();
    assert_eq!(viewers.len(), 1);
    assert_eq!(viewers[0].name, "csharp-viewer");
    // (which family the host lists is the one whose PUNCH got there first, as in 0.2.4: the viewer's choice,
    // IPv6, is where the data goes, but the listed path is not updated for it)
    assert!(!viewers[0].path.is_empty());
    assert_eq!((viewers[0].latency_ms, viewers[0].overhead_percent), (300, 25));
    assert!(!viewers[0].wants_control);
    assert!(!host.send_side(&viewers[0].sid, b"would end up in mpv"));
    assert_eq!(*side_datagrams.lock().unwrap(), 0);
    assert!(viewer.has(|line| line.starts_with("LOG [p2p] connected to rust-host: IPv6 [::1]:")), "{}", viewer.output());

    // the viewer leaves: this host hears its BYE
    assert!(viewer.finish(), "the 0.2.4 viewer did not stop:\n{}", viewer.output());
    assert!(wait_for(|| host.viewers().is_empty(), 5000), "{:?}", host.viewers());
    assert!(dump(&log).contains("[p2p] csharp-viewer left (the other side ended the session)"), "{}", dump(&log));

    host.stop();
    ts.close();
    drop(encoder);
}

#[test]
fn a_viewer_of_this_crate_watches_a_0_2_4_host() {
    const TEST: &str = "a_viewer_of_this_crate_watches_a_0_2_4_host";
    let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some((release, ffmpeg, ffprobe, server)) = prerequisites(TEST) else { return };
    let group_invite = Invite::create("interop", Some(&server.url())).unwrap();
    let code = invite::encode(&group_invite);
    let Some(mut host) = CSharp::start(TEST, &release.join("HdrCastSender.exe"), "host", &code, "none", server.echo_port, &ffmpeg) else { return };
    println!("{TEST}: the host is {}", release.join("HdrCastSender.exe").display());
    assert!(wait_for(|| host.latest("STATUS ").is_some_and(|status| status.starts_with("Live in \"interop\"")), 30_000), "the 0.2.4 host did not go live:\n{}", host.output());
    let host_id = host.latest("HOSTID ").expect("the host says who it is");

    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let viewer = ViewerSession::new(
        ViewerSessionOptions {
            group: GroupConfig::default().normalize().adopt(&group_invite, "rust-viewer"),
            host_id,
            latency_ms: 300,
            overhead_percent: 25,
            addresses: Some(loopback_firewalled()),
            echo_port: server.echo_port,
            map_router_port: false, // never the real router
            // the join carries "rc": a 0.2.4 host must read past it
            request_control: true,
            ..ViewerSessionOptions::default()
        },
        logger(&log),
    );
    let side_datagrams = Arc::new(Mutex::new(0usize));
    let counter = side_datagrams.clone();
    viewer.set_side_handler(Arc::new(move |_| *counter.lock().unwrap() += 1));
    viewer.start().unwrap();
    assert!(
        wait_for(|| viewer.state() == ViewerSessionState::Connected, 30_000),
        "this viewer did not connect to the 0.2.4 host: {}\n{}\n--- host ---\n{}",
        viewer.status(),
        dump(&log),
        host.output()
    );
    assert!(viewer.status().starts_with("Connected to csharp-host (IPv6 [::1]:"), "{}", viewer.status());
    assert_eq!(viewer.host_name(), "csharp-host");

    // the picture arrives: this crate's ViewerLink carries the 0.2.4 host's SRT
    let result = probe(&ffprobe, viewer.local_port(), &viewer.srt_passphrase());
    assert!(result.contains("codec_name=hevc"), "{result}\n{}\n--- host ---\n{}", dump(&log), host.output());
    assert!(result.contains("profile=Main 10"), "{result}");
    assert!(result.contains("color_transfer=smpte2084"), "{result}");

    // a 0.2.4 host offers no remote control, so this viewer sends it nothing on the side channel
    assert!(!viewer.host_offers_control());
    assert!(!viewer.send_side(b"would end up in ffmpeg"));
    assert!(viewer.sid().is_some());
    assert_eq!(*side_datagrams.lock().unwrap(), 0);
    assert!(
        wait_for(|| host.latest("STATUS ").is_some_and(|status| status.ends_with("|rust-viewer:Watching")), 5000),
        "the 0.2.4 host does not see this viewer watching:\n{}",
        host.output()
    );
    assert!(host.has(|line| line.starts_with("LOG [p2p] rust-viewer connected via IPv")), "{}", host.output());
    assert!(host.has(|line| line.starts_with("LOG [p2p] rust-viewer is joining (latency 300 ms, candidates [::1]:")), "{}", host.output());

    // the host stops: this viewer hears its BYE and waits for it to come back
    assert!(host.finish(), "the 0.2.4 host did not stop:\n{}", host.output());
    assert!(wait_for(|| viewer.state() == ViewerSessionState::Waiting, 8000), "{}", viewer.status());
    assert_eq!(viewer.status(), "The stream ended - waiting for csharp-host to go live again");
    viewer.stop();
}
