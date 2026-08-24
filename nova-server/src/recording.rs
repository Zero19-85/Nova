//! Local stream recording — the Rust half.
//!
//! The muxing lives in `shim/recorder.cpp`; this module decides *when* and
//! *where*, and is the only thing the tray talks to.
//!
//! ## Why this lives in the Worker
//!
//! The recorder is fed from inside `EncodeFrame`, and `EncodeFrame` runs in the
//! **Worker** — the Master owns no encoder and never sees a frame that is not
//! already sealed and packetised. A tray item is a Worker-side control too, so
//! the whole feature is same-process and needs no IPC.
//!
//! That is also why recording stops on a Worker restart rather than surviving
//! one: a sign-out kills the Worker, and the file it was writing has no owner
//! afterwards. [`stop`] runs on the Worker's teardown path so what is left on
//! disk is a finalised file rather than an abandoned handle.
//!
//! ## What "recording" costs
//!
//! One `memcpy` of the encoded frame on the capture thread — a few hundred
//! kilobytes at most, into a queue — and a writer thread doing the I/O. There is
//! no second encode, no decode, no GPU work at all. The stream a client receives
//! is bit-identical whether recording is on or off, which is the property that
//! makes this safe to leave running.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::encoder;

/// Where recordings go, and what the last one was called.
///
/// A `Mutex` rather than atomics because it holds a path, and because the tray
/// is the only caller — this is pressed by a human at human rates.
static CURRENT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The directory recordings are written to.
///
/// Beside the executable, like every other piece of Nova's state
/// (`nova.toml`, `nova_paired.json`, `nova_display_baseline.txt`), and for the
/// same reason: the SCM sets a service's working directory to System32, so a
/// relative path writes somewhere nobody will look.
///
/// **Not** the user's Videos folder, tempting as that is. The Worker runs as the
/// elevated interactive user, but a Worker respawned under the SYSTEM fallback
/// does not have that user's profile — so a path derived from the profile would
/// resolve differently depending on which side of a sign-out the recording
/// started.
pub fn recordings_dir() -> PathBuf {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("recordings");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Whether a recording is open right now.
pub fn is_active() -> bool {
    encoder::recorder_is_active()
}

/// The file currently being written, if any.
pub fn current_path() -> Option<PathBuf> {
    CURRENT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Start recording the live session.
///
/// The stream's shape is read from [`crate::stats`] rather than taken as
/// parameters, and that is deliberate: `stats` is what the *encoder* published
/// after negotiation finished, so it is the only description guaranteed to match
/// the bytes that will arrive. A caller passing its own idea of the geometry
/// would be passing what was requested, and this project has been bitten by that
/// distinction more than once (grants, codecs, and refresh rates are all things
/// the host is free to answer differently).
pub fn start() -> Result<PathBuf, String> {
    let snapshot = crate::stats::snapshot();
    if !snapshot.streaming {
        return Err("nothing is streaming — there is no encoded frame to record".into());
    }
    if is_active() {
        return Err("already recording".into());
    }

    // The IDENTIFIER, never the label.
    //
    // This was `Codec::from_str(snapshot.codec)` and it is the bug that made
    // every recording a 0-byte file: `snapshot.codec` is display text
    // ("HEVC Main10 HDR"), `from_str` matches "hevc", and its catch-all arm
    // answers H264 rather than failing. The muxer was then told H.264 for an
    // HEVC stream, searched for NAL types 7/8 in a bitstream carrying 32/33/34,
    // never found a parameter set, and so never wrote a track.
    //
    // `None` is refused rather than guessed at. There is no safe default here —
    // guessing is exactly what produced a file that looked fine until someone
    // tried to play it.
    let Some(codec) = snapshot.codec_kind else {
        return Err(format!(
            "the session's codec is not known yet ({}) — nothing to record",
            snapshot.codec
        ));
    };
    // `codec.as_str()` ("hevc"), not `snapshot.codec` ("HEVC Main10 HDR"). The
    // label contains spaces, which produced `…-HEVC Main10 HDR.mkv` — awkward to
    // type, awkward to quote in a shell, and a second place where a display
    // string had leaked into something structural.
    let path = recordings_dir().join(format!(
        "nova-{}-{}x{}-{}{}.mkv",
        timestamp(),
        snapshot.width,
        snapshot.height,
        codec.as_str(),
        if snapshot.hdr { "-hdr" } else { "" },
    ));

    match encoder::recorder_start(
        &path,
        codec,
        snapshot.width,
        snapshot.height,
        // The NEGOTIATED cadence, not the measured one: the container's
        // DefaultDuration describes what the stream is meant to be, and seeding
        // it from a one-second fps sample would bake that second's jitter into
        // every timestamp in the file.
        snapshot.target_fps,
        snapshot.hdr,
    ) {
        0 => {
            *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.clone());
            println!("⏺️  Recording to {}", path.display());
            Ok(path)
        }
        -1 => Err("already recording".into()),
        -2 => Err(format!("could not create {}", path.display())),
        -3 => Err(format!(
            "{} cannot be recorded yet — the writer handles H.264 and HEVC; AV1 needs its own \
             configuration record",
            snapshot.codec
        )),
        code => Err(format!("the recorder refused to start (code {code})")),
    }
}

/// Finish and close the recording.
///
/// Safe to call when nothing is recording, and safe to call on a teardown path —
/// which is where it mostly runs. A recording that is never stopped still leaves
/// a playable file (see `recorder.cpp` on why Matroska), but an unpatched one.
pub fn stop() -> Result<Option<PathBuf>, String> {
    let path = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).take();
    match encoder::recorder_stop() {
        0 => {
            let dropped = encoder::recorder_dropped_frames();
            if let Some(path) = &path {
                if dropped > 0 {
                    // Reported, never hidden. A recording with gaps is still
                    // worth having, but a user who is told it is clean and finds
                    // stutter will blame the encoder.
                    println!(
                        "⏹️  Recording saved to {} — {dropped} frame(s) were dropped because the \
                         writer could not keep up with the disk",
                        path.display()
                    );
                } else {
                    println!("⏹️  Recording saved to {}", path.display());
                }
            }
            Ok(path)
        }
        -1 => Ok(None),
        -2 => {
            println!(
                "⚠️  The recording was closed but its header could not be patched — the file \
                 plays, but a player will have to scan it to find its length"
            );
            Ok(path)
        }
        code => Err(format!("the recorder reported {code} while finalising")),
    }
}

/// Toggle, for the tray's single menu item.
pub fn toggle() -> Result<String, String> {
    if is_active() {
        match stop()? {
            Some(path) => Ok(format!("Saved {}", path.display())),
            None => Ok("Nothing was recording".into()),
        }
    } else {
        start().map(|path| format!("Recording to {}", path.display()))
    }
}

/// `20260823-142530`. Local time, because the person looking for the file is
/// the person who pressed the button.
fn timestamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_refuses_to_start_with_nothing_streaming() {
        let _g = crate::stats::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::stats::session_ended();
        // The error names the reason rather than the code, because this one is
        // shown to a person who pressed a button that looked available.
        let err = start().unwrap_err();
        assert!(err.contains("nothing is streaming"), "unexpected: {err}");
    }

    #[test]
    fn the_recordings_directory_is_beside_the_executable() {
        // Not the user profile — see the doc comment. A Worker respawned under
        // the SYSTEM fallback resolves a profile path differently, which would
        // scatter one session's recordings across two directories.
        let dir = recordings_dir();
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        assert_eq!(dir.parent().unwrap(), exe_dir);
    }

    #[test]
    fn stopping_when_nothing_records_is_not_an_error() {
        // The teardown path calls this unconditionally.
        assert_eq!(stop().unwrap(), None);
    }
}
