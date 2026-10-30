//! latprobe: glass-to-glass latency on one PC, read off the screen (docs/design.md §11.5, windows-checklist §15).
//! Windows only. Build: `cargo build --release -p hdrcast-capture --example latprobe`.
//!
//! clock  --x X --y Y --cell C --seconds S
//!     A top-most, click-through window at virtual-screen (X, Y) that shows the current QPC time (0.1 ms units, 24 bits,
//!     with sync and parity cells) as a bar code, redrawn once per desktop composition. Put it on the display the host
//!     captures.
//! read   --output N --region x,y,w,h --seconds S [--dump file.ppm --dump-at K]
//!     Desktop Duplication of output N (an SDR one): every new desktop image, the bar code inside the region (output
//!     coordinates: where the viewer's player shows the clock) is read and its time compared with the image's present
//!     time. Prints the distribution.
//! place  --rect x,y,w,h
//!     Moves HdrCast's mpv window (class mpv, title HdrCast) there and prints its client area in screen pixels.
//! feed   --hevc FILE --fps F --output N --region x,y,w,h --mpv EXE [--place x,y,w,h] -- <mpv args>
//!     mpv alone: starts it on stdin, writes the file's access units at F fps (each followed by an AUD), and reads the
//!     frame number the bar code in each picture carries off the screen: written -> on screen. The file: frames whose
//!     top-left bar code carries their number (the layout of `clock`, 40 px cells), encoded with
//!     `-bsf:v hevc_metadata=aud=insert`.

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("latprobe runs on Windows only");
}

#[cfg(windows)]
mod imp {
    use std::collections::HashMap;
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use windows::core::*;
    use windows::Win32::Foundation::*;
    use windows::Win32::Graphics::Direct3D::*;
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dwm::DwmFlush;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Performance::*;
    use windows::Win32::UI::HiDpi::*;
    use windows::Win32::UI::WindowsAndMessaging::*;

    const BITS: usize = 24;
    const CELLS: usize = BITS + 3;
    const MASK: u32 = (1 << BITS) - 1;

    fn qpc() -> i64 {
        let mut v = 0;
        unsafe {
            let _ = QueryPerformanceCounter(&mut v);
        }
        v
    }

    fn qpf() -> i64 {
        let mut v = 0;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut v);
        }
        v
    }

    /// QPC ticks -> 0.1 ms units, 24 bits.
    fn units(ticks: i64, freq: i64) -> u32 {
        ((ticks as i128 * 10_000 / freq as i128) as u64 as u32) & MASK
    }

    fn encode(value: u32) -> [bool; CELLS] {
        let mut cells = [false; CELLS];
        cells[0] = true;
        cells[1] = false;
        let mut parity = false;
        for i in 0..BITS {
            let bit = (value >> i) & 1 == 1;
            cells[2 + i] = bit;
            parity ^= bit;
        }
        cells[CELLS - 1] = parity;
        cells
    }

    fn decode(cells: &[bool]) -> Option<u32> {
        if cells.len() != CELLS || !cells[0] || cells[1] {
            return None;
        }
        let mut value = 0u32;
        let mut parity = false;
        for i in 0..BITS {
            if cells[2 + i] {
                value |= 1 << i;
                parity = !parity;
            }
        }
        (parity == cells[CELLS - 1]).then_some(value)
    }

    fn arg<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
    }

    fn num(args: &[String], name: &str, default: i64) -> i64 {
        arg(args, name).map(|v| v.parse().expect(name)).unwrap_or(default)
    }

    fn region(args: &[String]) -> (u32, u32, u32, u32) {
        let v: Vec<u32> = arg(args, "--region").expect("--region x,y,w,h").split(',').map(|p| p.parse().unwrap()).collect();
        (v[0], v[1], v[2], v[3])
    }

    pub fn main() {
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let args: Vec<String> = std::env::args().collect();
        match args.get(1).map(String::as_str) {
            Some("clock") => clock(&args),
            Some("read") => {
                let stop = Arc::new(AtomicBool::new(false));
                let seconds = num(&args, "--seconds", 10) as u64;
                let s = stop.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_secs(seconds));
                    s.store(true, Ordering::SeqCst);
                });
                let freq = qpf();
                let samples = read(&args, &stop, &|value, present| {
                    let lat = units(present, freq).wrapping_sub(value) & MASK;
                    Some(lat as f64 / 10.0)
                });
                report("clock -> screen", &samples);
            }
            Some("feed") => feed(&args),
            Some("place") => {
                // place --rect x,y,w,h: moves HdrCast's mpv window (class mpv, title HdrCast) there, then prints its client
                // area in screen pixels
                let v: Vec<i32> = arg(&args, "--rect").expect("--rect").split(',').map(|p| p.parse().unwrap()).collect();
                place_window(w!("mpv"), w!("HdrCast"), v[0], v[1], v[2], v[3]);
                unsafe {
                    if let Ok(hwnd) = FindWindowW(w!("mpv"), w!("HdrCast")) {
                        let mut r = RECT::default();
                        let _ = GetClientRect(hwnd, &mut r);
                        let mut p = POINT { x: 0, y: 0 };
                        let _ = ClientToScreen(hwnd, &mut p);
                        println!("client {} {} {} {}", p.x, p.y, r.right, r.bottom);
                    } else {
                        println!("no mpv window");
                    }
                }
            }
            _ => eprintln!("latprobe clock|read|feed ..."),
        }
    }

    // ---- clock -------------------------------------------------------------------------------------------------------

    extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn clock(args: &[String]) {
        let x = num(args, "--x", 0) as i32;
        let y = num(args, "--y", 0) as i32;
        let cell = num(args, "--cell", 24) as i32;
        let seconds = num(args, "--seconds", 30) as u64;
        let freq = qpf();
        unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let class = w!("latprobe-clock");
            let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance.into(), lpszClassName: class, ..Default::default() };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                class,
                w!("latprobe"),
                WS_POPUP,
                x,
                y,
                cell * CELLS as i32,
                cell * 2,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA).unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let white = HBRUSH(GetStockObject(WHITE_BRUSH).0);
            let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
            let end = Instant::now() + Duration::from_secs(seconds);
            let mut frames = 0u64;
            while Instant::now() < end {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let cells = encode(units(qpc(), freq));
                let dc = GetDC(Some(hwnd));
                for (i, on) in cells.iter().enumerate() {
                    let r = RECT { left: i as i32 * cell, top: 0, right: (i as i32 + 1) * cell, bottom: cell * 2 };
                    FillRect(dc, &r, if *on { white } else { black });
                }
                ReleaseDC(Some(hwnd), dc);
                let _ = DwmFlush();
                frames += 1;
            }
            eprintln!("clock: {frames} frames drawn");
            let _ = DestroyWindow(hwnd);
        }
    }

    // ---- reading the screen ------------------------------------------------------------------------------------------

    /// Duplicates output `--output`, decodes the bar code in `--region` from every new desktop image and hands
    /// (value, present QPC) to `measure`; collects what it returns (one sample per new value).
    fn read(args: &[String], stop: &AtomicBool, measure: &dyn Fn(u32, i64) -> Option<f64>) -> Vec<f64> {
        let index = num(args, "--output", 1) as u32;
        let (rx, ry, rw, rh) = region(args);
        let dump = arg(args, "--dump").map(str::to_string);
        let dump_at = num(args, "--dump-at", 200) as u64;
        let mut samples = Vec::new();
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().unwrap();
            let mut found = None;
            let mut n = 0;
            'outer: for a in 0.. {
                let Ok(adapter) = factory.EnumAdapters1(a) else { break };
                for o in 0.. {
                    let Ok(output) = adapter.EnumOutputs(o) else { break };
                    if n == index {
                        found = Some((adapter.clone(), output));
                        break 'outer;
                    }
                    n += 1;
                }
            }
            let (adapter, output) = found.expect("no such output");
            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter.cast::<IDXGIAdapter>().unwrap(),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .unwrap();
            let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
            let output1: IDXGIOutput1 = output.cast().unwrap();
            let dup = output1.DuplicateOutput(&device).unwrap();
            let desc = dup.GetDesc();
            assert_eq!(desc.ModeDesc.Format, DXGI_FORMAT_B8G8R8A8_UNORM, "only SDR (BGRA8) outputs");
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: rw,
                Height: rh,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging)).unwrap();
            let staging = staging.unwrap();
            let mut last: Option<u32> = None;
            let mut dumped = false;
            let (mut images, mut undecoded) = (0u64, 0u64);
            while !stop.load(Ordering::SeqCst) {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource = None;
                match dup.AcquireNextFrame(50, &mut info, &mut resource) {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
                    Err(e) => panic!("acquire: {e}"),
                }
                if info.LastPresentTime != 0 {
                    images += 1;
                    let texture: ID3D11Texture2D = resource.unwrap().cast().unwrap();
                    let bx = D3D11_BOX { left: rx, top: ry, front: 0, right: rx + rw, bottom: ry + rh, back: 1 };
                    context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &texture, 0, Some(&bx));
                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                    context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
                    let pitch = mapped.RowPitch as usize;
                    let data = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * rh as usize);
                    if let (Some(path), false, true) = (&dump, dumped, images >= dump_at) {
                        let mut ppm = format!("P6\n{rw} {rh}\n255\n").into_bytes();
                        for y in 0..rh as usize {
                            for x in 0..rw as usize {
                                let p = &data[y * pitch + x * 4..];
                                ppm.extend_from_slice(&[p[2], p[1], p[0]]);
                            }
                        }
                        std::fs::write(path, ppm).unwrap();
                        dumped = true;
                    }
                    let cw = rw as f64 / CELLS as f64;
                    let cy = rh as usize / 2;
                    let cells: Vec<bool> = (0..CELLS)
                        .map(|i| {
                            let cx = ((i as f64 + 0.5) * cw) as usize;
                            let p = &data[cy * pitch + cx * 4..];
                            (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3 > 128
                        })
                        .collect();
                    context.Unmap(&staging, 0);
                    match decode(&cells) {
                        Some(value) if Some(value) != last => {
                            last = Some(value);
                            if let Some(ms) = measure(value, info.LastPresentTime) {
                                samples.push(ms);
                            }
                        }
                        Some(_) => {}
                        None => undecoded += 1,
                    }
                }
                let _ = dup.ReleaseFrame();
            }
            eprintln!("read: {images} desktop images, {undecoded} without a readable code, {} samples", samples.len());
        }
        samples
    }

    fn report(what: &str, samples: &[f64]) {
        if samples.is_empty() {
            println!("{what}: no samples");
            return;
        }
        let mut s = samples.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |q: f64| s[((s.len() - 1) as f64 * q).round() as usize];
        let mean = s.iter().sum::<f64>() / s.len() as f64;
        println!(
            "{what}: n={} min={:.1} p10={:.1} median={:.1} mean={:.1} p90={:.1} p99={:.1} max={:.1} ms",
            s.len(),
            s[0],
            at(0.1),
            at(0.5),
            mean,
            at(0.9),
            at(0.99),
            s[s.len() - 1]
        );
    }

    // ---- feeding mpv -------------------------------------------------------------------------------------------------

    /// Moves the window (class, title) to virtual-screen pixels once it exists, and again a moment later (a move to a
    /// monitor with another DPI makes the window resize itself).
    fn place_window(class: PCWSTR, title: PCWSTR, x: i32, y: i32, w: i32, h: i32) {
        let begun = Instant::now();
        let mut placed = 0;
        while begun.elapsed() < Duration::from_secs(8) && placed < 3 {
            unsafe {
                if let Ok(hwnd) = FindWindowW(class, title) {
                    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
                    placed += 1;
                    std::thread::sleep(Duration::from_millis(400));
                    continue;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    const AUD: [u8; 7] = [0, 0, 0, 1, 0x46, 0x01, 0x50];

    /// Splits an Annex B stream whose access units each start with an AUD (hevc_metadata=aud=insert) into the units,
    /// without their AUD.
    fn access_units(stream: &[u8]) -> Vec<Vec<u8>> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 5 < stream.len() {
            if stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 1 && (stream[i + 3] >> 1) & 0x3f == 35 {
                let start = if i > 0 && stream[i - 1] == 0 { i - 1 } else { i };
                starts.push((start, i + 3 + 3)); // the AUD is 3 bytes after the start code: header (2) + payload (1)
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut units = Vec::new();
        for (n, &(_, body)) in starts.iter().enumerate() {
            let end = starts.get(n + 1).map_or(stream.len(), |&(next, _)| next);
            units.push(stream[body..end].to_vec());
        }
        units
    }

    fn feed(args: &[String]) {
        let file = std::fs::read(arg(args, "--hevc").expect("--hevc")).unwrap();
        let units = access_units(&file);
        let fps = num(args, "--fps", 60) as f64;
        let mpv = arg(args, "--mpv").expect("--mpv").to_string();
        let rest = args.iter().position(|a| a == "--").map(|i| args[i + 1..].to_vec()).unwrap_or_default();
        eprintln!("feed: {} access units at {fps} fps", units.len());
        let mut child = Command::new(&mpv).args(&rest).stdin(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let written: Arc<Mutex<HashMap<u32, i64>>> = Arc::new(Mutex::new(HashMap::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let warmup = num(args, "--warmup-ms", 1500) as u64;

        if let Some(place) = arg(args, "--place") {
            let v: Vec<i32> = place.split(',').map(|p| p.parse().unwrap()).collect();
            std::thread::spawn(move || place_window(w!("mpv"), w!("latprobe"), v[0], v[1], v[2], v[3]));
        }
        let (w, s) = (written.clone(), stop.clone());
        let writer = std::thread::spawn(move || {
            let interval = Duration::from_secs_f64(1.0 / fps);
            let start = Instant::now();
            for (i, unit) in units.iter().enumerate() {
                let due = start + interval * i as u32;
                loop {
                    let now = Instant::now();
                    if now >= due {
                        break;
                    }
                    let left = due - now;
                    if left > Duration::from_millis(2) {
                        std::thread::sleep(left - Duration::from_millis(1));
                    }
                }
                w.lock().unwrap().insert(i as u32, qpc());
                if stdin.write_all(unit).and_then(|_| stdin.write_all(&AUD)).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(500));
            s.store(true, Ordering::SeqCst);
            drop(stdin);
        });
        let freq = qpf();
        let begun = qpc();
        let samples = read(args, &stop, &|value, present| {
            let at = *written.lock().unwrap().get(&value)?;
            if (at - begun) * 1000 / freq < warmup as i64 {
                return None;
            }
            Some((present - at) as f64 * 1000.0 / freq as f64)
        });
        writer.join().unwrap();
        let _ = child.kill();
        report("write -> screen", &samples);
    }
}
