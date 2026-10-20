//! Sender pipeline benchmark: a full `CastSession` (capture -> NV12 -> encoder -> UDP) cast to
//! a local UDP sink, measuring this process's CPU time (GetProcessTimes) and the session stats.
//!
//! The picture comes from a test window drawn by a child process (so its drawing is not
//! counted): `--scene motion` changes the whole window every ~16 ms, `--scene static` draws
//! it once. `--source monitor` captures monitor 1 instead (the window is kept on top).
//!
//!   cargo run --release -p nanocast-win --example pipeline_bench -- [--scene motion|static]
//!       [--source window|monitor] [--seconds 10] [--fps 60] [--mbps 20] [--cores N]
//!       [--skip-cpus K] [--to 127.0.0.1:48322]   (a real receiver, e.g. `nanocast-receiver --windowed`)
//!
//! The path is chosen like in the app: `NANOCAST_CONVERT=gpu|cpu`, `NANOCAST_SKIP_STATIC=0`.
//! `--cores N` pins the process to N logical CPUs (a rough "weak PC"), from CPU `--skip-cpus`.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    bench::main()
}

#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
mod bench {
    use std::io::{BufRead, BufReader};
    use std::net::UdpSocket;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context as _, Result};
    use nanocast_core::protocol::VideoCodec;
    use nanocast_win::capture::Source;
    use nanocast_win::session::{CastConfig, CastSession, CastStats, ConvertPath, Phase};
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::{COLORREF, FILETIME, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes, SetProcessAffinityMask};
    use windows::Win32::UI::WindowsAndMessaging::*;

    struct Args {
        scene: String,
        source: String,
        seconds: u64,
        fps: u32,
        mbps: u32,
        cores: Option<u32>,
        /// With `--cores`: leave out the first logical CPUs (CPU 0 also serves interrupts).
        skip_cpus: u32,
        /// Receiver (video port; control on the next port) instead of the local sink.
        to: Option<std::net::SocketAddr>,
        window_only: bool,
    }

    fn parse() -> Result<Args> {
        let mut a = Args {
            scene: "motion".into(),
            source: "window".into(),
            seconds: 10,
            fps: 60,
            mbps: 20,
            cores: None,
            skip_cpus: 0,
            to: None,
            window_only: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(k) = it.next() {
            let mut v = || it.next().context("missing value");
            match k.as_str() {
                "--scene" => a.scene = v()?,
                "--source" => a.source = v()?,
                "--seconds" => a.seconds = v()?.parse()?,
                "--fps" => a.fps = v()?.parse()?,
                "--mbps" => a.mbps = v()?.parse()?,
                "--cores" => a.cores = Some(v()?.parse()?),
                "--skip-cpus" => a.skip_cpus = v()?.parse()?,
                "--to" => a.to = Some(v()?.parse()?),
                "--window-only" => a.window_only = true,
                other => bail!("unknown argument {other}"),
            }
        }
        Ok(a)
    }

    pub fn main() -> Result<()> {
        let args = parse()?;
        if args.window_only {
            return test_window(&args.scene, args.source == "monitor");
        }
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
        if let Some(n) = args.cores {
            // Before rayon starts its pool.
            std::env::set_var("RAYON_NUM_THREADS", n.to_string());
            unsafe { SetProcessAffinityMask(GetCurrentProcess(), ((1usize << n) - 1) << args.skip_cpus) }.context("SetProcessAffinityMask")?;
        }

        // The test window lives in a child process; it prints its HWND.
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--window-only", "--scene", &args.scene, "--source", &args.source])
            .stdout(Stdio::piped())
            .spawn()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)?;
        let hwnd: isize = line.trim().parse().context("child HWND")?;
        std::thread::sleep(Duration::from_millis(500));

        let sink = UdpSocket::bind("127.0.0.1:0")?;
        let sink_addr = sink.local_addr()?;
        let received = Arc::new(AtomicU64::new(0));
        {
            let received = received.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 2048];
                while let Ok(n) = sink.recv(&mut buf) {
                    received.fetch_add(n as u64, Ordering::Relaxed);
                }
            });
        }

        let source = if args.source == "monitor" { Source::Monitor(1) } else { Source::Window(hwnd) };
        let mut session = CastSession::start(CastConfig {
            video_addr: args.to.unwrap_or(sink_addr),
            // Without --to nothing listens there: UDP only.
            ctrl_port: args.to.map_or(1, |a| a.port() + 1),
            source,
            fps: args.fps,
            bitrate_mbps: args.mbps,
            encoder: None,
            codec: VideoCodec::H264,
            adaptive: false,
            pin: None,
            pace_mbps: 400,
            sender_name: "bench".into(),
            audio: false,
            convert: ConvertPath::Auto,
            skip_static: true,
        });

        // Warm up (control channel timeout, encoder start, first IDR).
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            let st = session.stats();
            match &st.phase {
                Phase::Failed(e) => bail!("cast failed: {e}"),
                Phase::Ended(r) => bail!("cast ended: {r:?}"),
                Phase::Casting if st.frames_sent > 0 => break,
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_secs(2));

        let s0 = session.stats();
        let cpu0 = cpu_time_100ns();
        let bytes0 = received.load(Ordering::Relaxed);
        let t0 = Instant::now();
        let mut samples: Vec<CastStats> = Vec::new();
        while t0.elapsed() < Duration::from_secs(args.seconds) {
            std::thread::sleep(Duration::from_millis(500));
            samples.push(session.stats());
        }
        let wall = t0.elapsed().as_secs_f64();
        let cpu = (cpu_time_100ns() - cpu0) as f64 / 1e7;
        let bytes = received.load(Ordering::Relaxed) - bytes0;
        let s1 = session.stats();
        session.stop();
        let _ = child.kill();

        let flowing: Vec<&CastStats> = samples.iter().filter(|s| s.fps > 0.0).collect();
        let avg = |f: fn(&CastStats) -> f64| {
            if flowing.is_empty() {
                0.0
            } else {
                flowing.iter().map(|s| f(s)).sum::<f64>() / flowing.len() as f64
            }
        };
        let cores = args.cores.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() as u32));
        let sent = s1.frames_sent - s0.frames_sent;
        println!();
        println!("encoder {} | NV12 on {} | scene {} | source {} | {} cores", s1.encoder, if s1.gpu_convert { "GPU" } else { "CPU" }, args.scene, args.source, cores);
        println!(
            "CPU {:.1}% of one core ({:.2}% of {cores}) | sent {:.1} fps, {:.2} Mbps on the wire | unchanged {} repeated {} dropped {}",
            cpu / wall * 100.0,
            cpu / wall * 100.0 / cores as f64,
            sent as f64 / wall,
            bytes as f64 * 8.0 / wall / 1e6,
            s1.frames_unchanged - s0.frames_unchanged,
            s1.frames_repeated - s0.frames_repeated,
            s1.frames_dropped - s0.frames_dropped,
        );
        println!(
            "convert {:.2} ms | encode {:.2} ms | capture->encoded {:.2} ms (averages of 0.5 s windows with frames)",
            avg(|s| s.convert_ms),
            avg(|s| s.encode_ms),
            avg(|s| s.latency_ms)
        );
        Ok(())
    }

    fn cpu_time_100ns() -> u64 {
        let (mut c, mut e, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe {
            let _ = GetProcessTimes(GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u);
        }
        let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        ft(k) + ft(u)
    }

    static FRAME: AtomicU32 = AtomicU32::new(0);

    fn draw(hwnd: HWND) {
        unsafe {
            let n = FRAME.load(Ordering::Relaxed);
            let hdc = GetDC(Some(hwnd));
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            // Moving coloured stripes: the whole picture changes every frame (worst case).
            let stripes = 16;
            let sw = (rc.right / stripes).max(1);
            for i in -1..=stripes {
                let x = i * sw + (n as i32 * 6) % sw;
                let c = (i.rem_euclid(stripes) as u32 * 37 + n) % 255;
                let brush = CreateSolidBrush(COLORREF(c | ((255 - c) << 8) | (((c * 3) % 255) << 16)));
                FillRect(hdc, &RECT { left: x, top: 0, right: x + sw, bottom: rc.bottom }, brush);
                let _ = DeleteObject(brush.into());
            }
            // Text, like a slide or code.
            let text: Vec<u16> = format!("NANOCast bench frame {n}").encode_utf16().collect();
            let _ = TextOutW(hdc, 40, 40 + (n as i32 % 200), &text);
            ReleaseDC(Some(hwnd), hdc);
        }
    }

    extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            match msg {
                WM_PAINT => {
                    let mut ps = PAINTSTRUCT::default();
                    BeginPaint(hwnd, &mut ps);
                    let _ = EndPaint(hwnd, &ps);
                    draw(hwnd);
                    LRESULT(0)
                }
                WM_DESTROY => {
                    PostQuitMessage(0);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wp, lp),
            }
        }
    }

    fn test_window(scene: &str, topmost: bool) -> Result<()> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("NanoCastBench");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let ex = if topmost { WS_EX_TOPMOST } else { WINDOW_EX_STYLE(0) };
            let hwnd = CreateWindowExW(
                ex,
                class,
                PCWSTR(w!("NANOCast bench").0),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                1280,
                720,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            println!("{}", hwnd.0 as isize);
            let motion = scene == "motion";
            let mut msg = MSG::default();
            loop {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    if msg.message == WM_QUIT {
                        return Ok(());
                    }
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if motion {
                    FRAME.fetch_add(1, Ordering::Relaxed);
                    draw(hwnd);
                }
                std::thread::sleep(Duration::from_millis(16));
            }
        }
    }
}
