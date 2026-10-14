/// Nova file logger.
///
/// Strategy:
///   Rust side  — `SetStdHandle(STD_OUTPUT/ERROR_HANDLE, ..)` redirects ALL
///                subsequent `println!` / `eprintln!` in the entire process.
///                Zero changes to existing call sites needed.
///
///                The handle is NOT the log file. It is the write end of an
///                in-process pipe: `nova-log-drain` empties the pipe into a
///                bounded memory backlog, and `nova-log-writer` is the only
///                thread that ever touches the file. **No log line can block
///                the thread that printed it on the disk.** That matters because
///                `println!` runs on the hot path — `📊 RTP/s` inside the
///                Master's send thread, `🎞 Encoder output` in the Worker's
///                capture loop — and Windows throttles EVERY buffered writer on
///                the machine while dirty cache pages exceed its threshold.
///                Measured live 2026-10-14 during a 1.2 Gbps game download onto
///                an SSD that had run out of fast cache: ~20 s with every cached
///                write blocked, every ~40 s, and Nova's NIC output at 0.00 MB/s
///                for each of them — the stream froze inside a log line.
///
///   C shim side — The CRT's FILE* descriptors are independent of the Win32
///                 handle table, so `printf()` does NOT follow `SetStdHandle`.
///                 `InitShimLogHandle` hands `shim.cpp` the same pipe, and it
///                 `_dup2`s the CRT stdout/stderr onto it. (`InitShimLog` with a
///                 path — the shim opening the file itself — is the fallback
///                 when the pipe could not be built.)
///
/// Log location: `{exe_dir}\nova.log`  (same directory as the executable).
/// In a Windows Service the SCM sets CWD = System32, so a relative path would
/// silently write (or fail) there.  Anchoring to the exe directory keeps the
/// log next to the binary regardless of how the process was started.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_ALWAYS,
};
use windows::Win32::System::Console::{
    SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Pipes::{CreatePipe, PeekNamedPipe};

// ── Log path ─────────────────────────────────────────────────────────────────

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn log_path() -> PathBuf {
    exe_dir().join("nova.log")
}

/// The launcher **service** logs to its own file. Critical: the service holds
/// its log handle open for its whole life, and the host it spawns opens ITS
/// log at startup. If both targeted `nova.log`, the host's open would collide
/// with the service's (even with FILE_SHARE_WRITE the two would interleave
/// unreadably). Separate files keep each process's log clean and, more
/// importantly, guarantee the host can always open its own.
pub fn service_log_path() -> PathBuf {
    exe_dir().join("nova-service.log")
}

/// The SYSTEM input helper (`--system-input-helper`) logs to its own file, for
/// the same reason the service does: the Worker holds `nova.log` open for its
/// whole life, and a helper spawns/dies inside that window. A third file keeps
/// the secure-desktop injection trail readable on its own.
pub fn input_helper_log_path() -> PathBuf {
    exe_dir().join("nova-input.log")
}

/// Log path encoded as a null-terminated UTF-16 string for the C shim.
pub fn log_path_wide() -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    log_path()
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect()
}

// ── Rotation ─────────────────────────────────────────────────────────────────

/// Rotate once a log passes this size. Nova logs roughly 2 lines/second while
/// streaming (`📊 RTP/s`), so an always-on host produced ~13 MB/day and grew
/// without bound — the log was 16 MB before this existed.
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

/// Rotated generations kept beside the live log (`nova.log.1`, `nova.log.2`).
/// Two is enough to still hold the *previous* session when a crash is found
/// after the fact, and bounds each log family at 48 MB.
const KEPT_GENERATIONS: usize = 2;

/// How often the watchdog re-checks the size. A process can stream for days,
/// so rotating only at startup would not actually bound anything.
const ROTATE_CHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// Called after a rotation so a subsystem holding its own handle to the log can
/// reopen it. The C++ shim keeps a CRT handle that Win32 `SetStdHandle` does
/// not reach, so without this it would keep writing into the rotated-away file.
static REOPEN_HOOK: OnceLock<fn()> = OnceLock::new();

/// Register a reopen callback (see [`REOPEN_HOOK`]). First caller wins; the
/// shim registers itself when it initialises its log.
pub fn set_log_reopen_hook(hook: fn()) {
    let _ = REOPEN_HOOK.set(hook);
}

fn generation_path(path: &Path, n: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{n}"));
    PathBuf::from(name)
}

/// Shift `log.1 → log.2`, drop the oldest, and move the live file aside.
///
/// Every step is best-effort: a rotation that cannot happen (file locked by a
/// viewer, permissions) must never stop the process from logging, so failures
/// leave the current file in place and the next check simply tries again.
fn rotate(path: &Path) {
    let _ = std::fs::remove_file(generation_path(path, KEPT_GENERATIONS));
    for n in (1..KEPT_GENERATIONS).rev() {
        let _ = std::fs::rename(generation_path(path, n), generation_path(path, n + 1));
    }
    let _ = std::fs::rename(path, generation_path(path, 1));
}

fn oversized(path: &Path) -> bool {
    std::fs::metadata(path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false)
}

/// Open (or reopen) the log and point the process-wide stdout/stderr at it.
///
/// `FILE_SHARE_DELETE` is what makes live rotation possible at all: without it
/// the rename below fails for as long as the process holds the handle, which is
/// its entire life.
fn open_and_redirect(path: &Path) -> Option<HANDLE> {
    let handle = unsafe {
        CreateFileW(
            &windows::core::HSTRING::from(path.as_os_str()),
            0x0004u32, // FILE_APPEND_DATA — CreateFileW takes raw u32, not FILE_ACCESS_RIGHTS
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    match handle {
        Ok(h) => {
            unsafe {
                let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
                let _ = SetStdHandle(STD_ERROR_HANDLE, h);
            }
            Some(h)
        }
        Err(e) => {
            eprintln!("[Nova] WARNING: cannot open log file {}: {:?}", path.display(), e);
            None
        }
    }
}

/// Watch the live log and rotate it when it outgrows [`MAX_LOG_BYTES`].
///
/// Rotation is rename-then-reopen rather than truncate-in-place: truncating
/// would discard the whole history at the instant it crossed the threshold,
/// which is reliably the moment before you needed it.
fn spawn_rotation_watchdog(path: PathBuf, initial: HANDLE) {
    // `HANDLE` wraps a raw pointer and so is not `Send`. The value itself is
    // just a kernel handle-table index, valid process-wide, so it moves across
    // the thread boundary as an integer and is rebuilt on the far side.
    let initial = initial.0 as isize;
    std::thread::Builder::new()
        .name("nova-log-rotate".into())
        .spawn(move || {
            let mut current = HANDLE(initial as *mut core::ffi::c_void);
            loop {
                std::thread::sleep(ROTATE_CHECK);
                if !oversized(&path) {
                    continue;
                }
                rotate(&path);
                // Reopen even if the rename failed — the handle still points at
                // the old file either way, and a fresh open is harmless.
                if let Some(fresh) = open_and_redirect(&path) {
                    // Only now is the old handle unreferenced by stdout/stderr.
                    unsafe { let _ = CloseHandle(current); }
                    current = fresh;
                    println!("🗂️  Log rotated at {} (cap {} MB, keeping {} generation(s))",
                        timestamp(), MAX_LOG_BYTES / (1024 * 1024), KEPT_GENERATIONS);
                    if let Some(hook) = REOPEN_HOOK.get() {
                        hook();
                    }
                }
            }
        })
        .ok();
}

// ── Non-blocking sink ────────────────────────────────────────────────────────

/// Log output held in memory while the file write is stalled. Nova logs a few
/// hundred bytes a second while streaming, so this covers hours of stall — it
/// exists to bound a pathological case (a stuck volume plus a log storm), not
/// because the ~20 s throttling windows come anywhere near it.
const MAX_BACKLOG_BYTES: usize = 8 * 1024 * 1024;

/// Pipe buffer. Only has to absorb what arrives between two reads by
/// `nova-log-drain`, which does no I/O of its own besides that read.
const PIPE_BUFFER_BYTES: u32 = 1024 * 1024;

struct Backlog {
    chunks:  VecDeque<Vec<u8>>,
    bytes:   usize,
    /// Bytes refused because the backlog was full; reported by the writer.
    dropped: u64,
    /// The writer has taken a batch and not finished writing it.
    writing: bool,
}

struct LogSink {
    // HANDLEs as integers: kernel handle-table values are process-wide, and a
    // raw-pointer HANDLE is neither Send nor Sync.
    pipe_read:  isize,
    pipe_write: isize,
    backlog:    Mutex<Backlog>,
    ready:      Condvar,
    /// Both threads are running. Until then nothing may be told to write into
    /// the pipe — an undrained pipe blocks its writers once full.
    active:     AtomicBool,
}

static SINK: OnceLock<LogSink> = OnceLock::new();

impl LogSink {
    fn lock(&self) -> MutexGuard<'_, Backlog> {
        // A panic while holding this lock must not take logging down with it.
        self.backlog.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn active_sink() -> Option<&'static LogSink> {
    SINK.get().filter(|s| s.active.load(Ordering::Acquire))
}

/// The write end of the log pipe, for the C++ shim (`InitShimLogHandle`).
/// `None` when the logger fell back to writing the file directly.
pub fn log_sink_handle() -> Option<HANDLE> {
    active_sink().map(|s| HANDLE(s.pipe_write as *mut core::ffi::c_void))
}

fn open_log_file(path: &Path) -> std::io::Result<std::fs::File> {
    // FILE_SHARE_DELETE is what lets rotation rename the live file; READ and
    // WRITE keep `tail -f` and a second Nova process from ever being refused.
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .open(path)
}

/// Build the pipe, start both threads, and only then point stdout/stderr at
/// it. Returns `false` (leaving stdout untouched) if any step fails, so the
/// caller can fall back to the old direct-to-file redirect.
fn start_sink(path: PathBuf, file: std::fs::File) -> bool {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    if unsafe { CreatePipe(&mut read, &mut write, None, PIPE_BUFFER_BYTES) }.is_err() {
        return false;
    }
    let sink = LogSink {
        pipe_read:  read.0 as isize,
        pipe_write: write.0 as isize,
        backlog:    Mutex::new(Backlog { chunks: VecDeque::new(), bytes: 0, dropped: 0, writing: false }),
        ready:      Condvar::new(),
        active:     AtomicBool::new(false),
    };
    if SINK.set(sink).is_err() {
        unsafe {
            let _ = CloseHandle(read);
            let _ = CloseHandle(write);
        }
        return active_sink().is_some();
    }
    let sink = SINK.get().expect("just set");

    let writer = std::thread::Builder::new()
        .name("nova-log-writer".into())
        .spawn(move || writer_loop(sink, path, file));
    if writer.is_err() {
        return false;
    }
    let drain = std::thread::Builder::new()
        .name("nova-log-drain".into())
        .spawn(move || drain_loop(sink));
    if drain.is_err() {
        return false;
    }

    sink.active.store(true, Ordering::Release);
    unsafe {
        let _ = SetStdHandle(STD_OUTPUT_HANDLE, write);
        let _ = SetStdHandle(STD_ERROR_HANDLE, write);
    }
    install_panic_flush();
    true
}

/// Empties the pipe into the backlog as fast as lines arrive. Does no file I/O,
/// which is the whole point: while the writer is stuck behind the disk, this
/// thread keeps the pipe empty, so `println!` never waits.
fn drain_loop(sink: &'static LogSink) {
    // Wrapping the handle in a `File` is just a synchronous `ReadFile`.
    // ManuallyDrop: this must never close the handle `flush_log` peeks at.
    let mut pipe = std::mem::ManuallyDrop::new(unsafe {
        std::fs::File::from_raw_handle(sink.pipe_read as *mut core::ffi::c_void)
    });
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(0) => continue, // a zero-length write, not end of stream
            Ok(n) => n,
            Err(_) => return,  // every write end is gone — the process is exiting
        };
        {
            let mut b = sink.lock();
            if b.bytes + n > MAX_BACKLOG_BYTES {
                b.dropped += n as u64;
            } else {
                b.bytes += n;
                b.chunks.push_back(buf[..n].to_vec());
            }
        }
        sink.ready.notify_one();
    }
}

/// The only thread that writes the log file — and so the only one a stalled
/// disk can stop. Also owns rotation, which needs no `SetStdHandle` dance now
/// that nothing else holds the file.
fn writer_loop(sink: &'static LogSink, path: PathBuf, mut file: std::fs::File) {
    let mut last_rotate_check = Instant::now();
    loop {
        let (batch, dropped) = {
            let mut b = sink.lock();
            while b.chunks.is_empty() && b.dropped == 0 {
                let wait = ROTATE_CHECK.saturating_sub(last_rotate_check.elapsed());
                if wait.is_zero() {
                    break;
                }
                b = sink.ready.wait_timeout(b, wait).unwrap_or_else(|p| p.into_inner()).0;
            }
            let batch: Vec<Vec<u8>> = b.chunks.drain(..).collect();
            b.bytes = 0;
            let dropped = std::mem::take(&mut b.dropped);
            b.writing = !batch.is_empty() || dropped > 0;
            (batch, dropped)
        };

        for chunk in &batch {
            let _ = file.write_all(chunk);
        }
        if dropped > 0 {
            let _ = writeln!(file,
                "⚠️  Logger: {dropped} byte(s) of log output discarded — the log file \
                 stalled for longer than a {} MB backlog could cover",
                MAX_BACKLOG_BYTES / (1024 * 1024));
        }
        sink.lock().writing = false;

        if last_rotate_check.elapsed() >= ROTATE_CHECK {
            last_rotate_check = Instant::now();
            if oversized(&path) {
                rotate(&path);
                // Keep the old handle if the reopen fails — logging into a
                // renamed file beats not logging at all.
                if let Ok(fresh) = open_log_file(&path) {
                    file = fresh;
                    let _ = writeln!(file, "🗂️  Log rotated at {} (cap {} MB, keeping {} generation(s))",
                        timestamp(), MAX_LOG_BYTES / (1024 * 1024), KEPT_GENERATIONS);
                }
            }
        }
    }
}

/// Wait (bounded) until everything printed so far has reached the file.
///
/// Needed on the way OUT of the process: `process::exit` kills the logger
/// threads where they stand, and the last lines before an exit or a panic are
/// precisely the ones worth keeping. Two consecutive idle readings, because the
/// drain thread can hold a chunk it has read but not yet queued.
pub fn flush_log(timeout: Duration) {
    let Some(sink) = active_sink() else { return };
    let read = HANDLE(sink.pipe_read as *mut core::ffi::c_void);
    let deadline = Instant::now() + timeout;
    let mut idle_readings = 0;
    while Instant::now() < deadline {
        let mut in_pipe = 0u32;
        if unsafe { PeekNamedPipe(read, None, 0, None, Some(&mut in_pipe), None) }.is_err() {
            return;
        }
        let idle = {
            let b = sink.lock();
            b.chunks.is_empty() && b.dropped == 0 && !b.writing
        };
        if in_pipe == 0 && idle {
            idle_readings += 1;
            if idle_readings >= 2 {
                return;
            }
        } else {
            idle_readings = 0;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// [`flush_log`], then exit. Use instead of `std::process::exit` anywhere the
/// logger may be running.
pub fn exit(code: i32) -> ! {
    flush_log(Duration::from_secs(2));
    std::process::exit(code)
}

/// A panic message goes to stderr — the pipe — and the process may be gone
/// before the writer gets to it. Flush after the default hook has printed.
fn install_panic_flush() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        flush_log(Duration::from_secs(2));
    }));
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// Call ONCE, as the very first line of `run()` / `service_main()`, BEFORE
/// any `println!`.  Opens the log file and redirects the process-wide Win32
/// stdout + stderr handles so that all subsequent `println!` / `eprintln!`
/// anywhere in the Rust code — including on spawned threads — write to the
/// log file instead of the (absent) console.
pub fn init_debug_logger() {
    init_logger_to(log_path());
}

/// Logger init for the launcher **service** (`--service`). Uses a separate file
/// ([`service_log_path`]) so it never collides with the host's `nova.log`.
pub fn init_service_logger() {
    init_logger_to(service_log_path());
}

/// Logger init for the SYSTEM input helper (`--system-input-helper`) — see
/// [`input_helper_log_path`].
pub fn init_input_helper_logger() {
    init_logger_to(input_helper_log_path());
}

fn init_logger_to(path: PathBuf) {
    // A process initialises its logger once; `run()` re-calling it after the
    // binary already has is a no-op rather than a second pipe and writer.
    if active_sink().is_some() {
        return;
    }

    // Ensure parent directory exists (it should — exe is already there).
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    // Rotate before opening, so a restart never inherits an already-huge file
    // and start-up cost stays bounded.
    if oversized(&path) {
        rotate(&path);
    }

    // Open in append mode so multiple restarts accumulate in one file.
    // FILE_SHARE_READ lets an external viewer (`tail -f`) follow the log live;
    // WRITE means opening can never sharing-violation just because another Nova
    // process (service ↔ host, or a lingering instance) still has it open — the
    // open always succeeds so a process is never left silently logless.
    let via_pipe = match open_log_file(&path) {
        Ok(file) => start_sink(path.clone(), file),
        Err(e) => {
            eprintln!("[Nova] WARNING: cannot open log file {}: {:?}", path.display(), e);
            false
        }
    };
    if !via_pipe {
        // Fallback: the pre-2026-10-14 direct redirect. Every log line is then
        // a synchronous file write again — correct, just not stall-proof.
        match open_and_redirect(&path) {
            Some(h) => spawn_rotation_watchdog(path.clone(), h),
            None => {
                eprintln!("[Nova] Service output will not be captured.");
                return;
            }
        }
    }

    // From this point on, println! writes to the log.
    println!();
    println!("══════════════════════════════════════════════════════════");
    println!("  Nova  started at {}", timestamp());
    println!("  Log   {}", path.display());
    println!("  PID   {}", std::process::id());
    println!("  Cap   {} MB, {} rotated generation(s) kept",
        MAX_LOG_BYTES / (1024 * 1024), KEPT_GENERATIONS);
    println!("  Mode  {}", if via_pipe {
        "non-blocking (pipe → nova-log-writer)"
    } else {
        "DIRECT — log writes can stall the stream"
    });
    println!("══════════════════════════════════════════════════════════");
}

// ── DLL path probe ────────────────────────────────────────────────────────────

/// Log the absolute on-disk path of nova_shim.dll and whether it actually
/// exists where we expect it.  This catches "stale DLL in System32" or
/// "wrong search path" issues immediately on service startup.
pub fn log_shim_dll_path() {
    let exe_dir = exe_dir();
    let expected = exe_dir.join("nova_shim.dll");

    println!("[Nova] Exe directory   : {}", exe_dir.display());
    println!("[Nova] nova_shim.dll   : {}", expected.display());

    if expected.exists() {
        // Read the file metadata so we can log size and modification time —
        // helps confirm "stale old DLL vs freshly compiled one" at a glance.
        match std::fs::metadata(&expected) {
            Ok(m) => {
                let size_kb = m.len() / 1024;
                let modified = m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| format_epoch(d.as_secs()))
                    .unwrap_or_else(|| "unknown".to_string());
                println!("[Nova]   ✅  exists  size={}KB  modified={}", size_kb, modified);
            }
            Err(e) => println!("[Nova]   ⚠️  exists but metadata failed: {}", e),
        }
    } else {
        println!("[Nova]   ❌  NOT FOUND — stream will fail to start");
        println!("[Nova]   Deploy nova_shim.dll alongside nova-server.exe");

        // Check if a copy is lurking somewhere on the DLL search path (System32 etc.)
        for dir in dll_search_dirs() {
            let candidate = dir.join("nova_shim.dll");
            if candidate.exists() {
                println!("[Nova]   ⚠️  Found stale copy at {} — this may be loaded instead!",
                    candidate.display());
            }
        }
    }
}

/// Common locations Windows searches for DLLs (simplified; the real search
/// order also includes the manifest redirects and SxS, which we skip here).
fn dll_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(sys) = std::env::var_os("SystemRoot") {
        dirs.push(Path::new(&sys).join("System32"));
        dirs.push(Path::new(&sys).join("SysWOW64"));
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for p in std::env::split_paths(&path_var) {
            dirs.push(p);
        }
    }
    dirs
}

// ── Legacy helpers ────────────────────────────────────────────────────────────

/// Writes a timestamped line to the log.  With `SetStdHandle` active, plain
/// `println!` already goes to the log file, so this function is just a
/// convenience wrapper for code that wants explicit timestamps.
pub fn debug_log(msg: &str) {
    println!("[{}] {}", timestamp(), msg);
}

// ── Timestamp ─────────────────────────────────────────────────────────────────

fn timestamp() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_epoch(d.as_secs()) + &format!(".{:03}", d.subsec_millis())
}

fn format_epoch(secs: u64) -> String {
    let (y, mo, dd, hh, mm, ss) = epoch_to_parts(secs);
    format!("{y}-{mo:02}-{dd:02} {hh:02}:{mm:02}:{ss:02} UTC")
}

fn epoch_to_parts(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let ss = (secs % 60) as u32;
    let mm = ((secs / 60) % 60) as u32;
    let hh = ((secs / 3600) % 24) as u32;
    let mut days = secs / 86400;

    let mut y = 1970u32;
    loop {
        let in_year = if is_leap(y) { 366 } else { 365 };
        if days < in_year { break; }
        days -= in_year;
        y += 1;
    }
    let month_lens = [31u64, if is_leap(y) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut mo = 0u32;
    let mut rem = days as u64;
    for &ml in &month_lens {
        if rem < ml { break; }
        rem -= ml;
        mo += 1;
    }
    (y, mo + 1, rem as u32 + 1, hh, mm, ss)
}

fn is_leap(y: u32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
