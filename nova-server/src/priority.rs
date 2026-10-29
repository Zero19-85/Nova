//! Scheduling priority while a stream is live.
//!
//! **Why** (measured 2026-10-29, 4K120 Echo/Xbox): with Rockstar's launcher
//! starting on the host, one encode took 103 ms instead of ~4, the Worker's
//! hand-off to the Master blocked for 204 ms, the Master's RTP send thread fell
//! 3 frames behind and dropped 106 frames over 7.4 s, and the client's RTT went
//! from ~10 to 104 ms -- on a still desktop, with nothing heavy in the stream.
//! Nova ran at normal priority on both the CPU and the GPU, so anything that
//! wanted the machine could take it. Sunshine guards the same three ways:
//!
//! 1. **GPU process priority** (`D3DKMTSetProcessSchedulingPriorityClass`) for
//!    the process that captures and encodes -- the Worker.
//! 2. **GPU thread priority 7** on the capture/encode device
//!    (`IDXGIDevice::SetGPUThreadPriority`).
//! 3. **HIGH CPU priority class while streaming**, NORMAL otherwise -- applied
//!    to both the Worker and the Master, because both were starved.
//!
//! **HIGH, never REALTIME, on the GPU.** Sunshine documents that REALTIME on
//! NVIDIA can freeze the encoder or crash the driver with HAGS enabled, and
//! drops to HIGH there. This box is an RTX 5070 with HAGS at its default (on
//! for the 50 series), so HIGH unconditionally is the same answer without a
//! HAGS probe to get wrong.
//!
//! Every call here is best-effort: a refusal is logged once and the stream
//! runs exactly as it did before.

use std::sync::atomic::{AtomicU8, Ordering};

use windows::core::{s, Interface};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::Win32::System::Threading::{
    GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
};

/// `D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH` (d3dkmthk.h).
const D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH: i32 = 4;

type SetProcessSchedulingPriorityClass = unsafe extern "system" fn(HANDLE, i32) -> i32;

/// Raise the GPU priority of this process and of `device`'s GPU thread.
/// Call once, on the device capture and encode share.
pub fn raise_gpu_priority(device: &ID3D11Device) {
    unsafe {
        let process = match LoadLibraryA(s!("gdi32.dll"))
            .ok()
            .and_then(|gdi| GetProcAddress(gdi, s!("D3DKMTSetProcessSchedulingPriorityClass")))
        {
            Some(f) => {
                let f: SetProcessSchedulingPriorityClass = std::mem::transmute(f);
                let status = f(GetCurrentProcess(), D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH);
                if status >= 0 { "high".to_string() } else { format!("refused (NTSTATUS 0x{:08X})", status as u32) }
            }
            None => "unavailable (no D3DKMTSetProcessSchedulingPriorityClass)".to_string(),
        };
        let thread = match device.cast::<IDXGIDevice>().and_then(|d| d.SetGPUThreadPriority(7)) {
            Ok(()) => "7".to_string(),
            Err(e) => format!("refused ({e})"),
        };
        println!("🚀 GPU priority: process {process}, capture/encode thread {thread}");
    }
}

/// 0 = never set, 1 = normal, 2 = high.
static CPU_CLASS: AtomicU8 = AtomicU8::new(0);

/// HIGH CPU priority class while a stream is live, NORMAL otherwise.
/// Idempotent and cheap: safe to call every loop pass.
pub fn set_streaming(live: bool) {
    let want = if live { 2 } else { 1 };
    if CPU_CLASS.swap(want, Ordering::Relaxed) == want {
        return;
    }
    let class = if live { HIGH_PRIORITY_CLASS } else { NORMAL_PRIORITY_CLASS };
    match unsafe { SetPriorityClass(GetCurrentProcess(), class) } {
        Ok(()) => println!(
            "🚀 CPU priority class → {} ({})",
            if live { "HIGH" } else { "NORMAL" },
            if live { "stream live" } else { "no stream" }
        ),
        Err(e) => println!("⚠️  CPU priority class change refused: {e}"),
    }
}
