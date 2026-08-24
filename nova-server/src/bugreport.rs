//! The hybrid bug reporter: one question, and everything the answer needs.
//!
//! "It went wrong" is the least actionable report Nova receives, and it is
//! almost the only kind that arrives — because the things that would make it
//! actionable are a log file the user does not know exists, a GPU model they
//! have no reason to mention, and a frame that never left the encoder. This
//! module asks the one question a human can answer and assembles the rest
//! silently.
//!
//! ## The shape: zip, then hand off to the browser
//!
//! 1. Ask the one question.
//! 2. Gather logs, hardware, stream telemetry and the last encoded frame.
//! 3. Redact, and write it all to a folder.
//! 4. Zip the folder to the user's Desktop.
//! 5. Open GitHub's New Issue form, prefilled with a Markdown template, with an
//!    Explorer window behind it showing the zip already selected.
//!
//! The user drags the zip in, describes anything Nova could not, and submits —
//! under their own account.
//!
//! **There is no token in this program and no server behind it.** That is the
//! standard open-source shape and it is chosen, not settled for: Nova ships as
//! an installer, so a compiled-in credential is readable by everyone who has a
//! copy and revocable only for all of them at once. An earlier draft carried an
//! intake-endpoint mode and a direct-token mode; both were deleted rather than
//! defaulted off, because a config field for a token is an invitation to put one
//! there.
//!
//! ## Consent is not a formality here
//!
//! The bundle contains a log that names the machine, the user, every paired
//! device, and every address the host has spoken to, plus a picture of whatever
//! was on screen. [`redact`] removes the worst of it, and the final step is a
//! human pressing submit on a page showing them exactly what they are sharing.
//! A reporter that uploaded a desktop screenshot on someone's behalf would be
//! doing something they never agreed to, however good the intent — so the manual
//! last step is the feature, not a limitation of the approach.
//!
//! ## Why PowerShell for the dialogs and the zip
//!
//! Precedent, and footprint. `tray.rs` already drives its pairing dialogs
//! through `Microsoft.VisualBasic.Interaction`, and the ViGEmBus bootstrap
//! already shells out to `Invoke-WebRequest`. `Compress-Archive` ships with
//! every supported Windows. Adding a zip crate and an HTTPS stack to the host
//! binary for one button would fight the project's minimal-exe goal for no gain
//! a user could perceive.

use std::path::{Path, PathBuf};

use crate::debug;

/// The project's issue tracker.
///
/// A constant rather than only a config default, so a `nova.toml` that predates
/// `[bugreport]` — or one where the field was blanked — still files against
/// somewhere real instead of building a URL with an empty owner.
pub const DEFAULT_REPOSITORY: &str = "Zero19-85/Nova";

/// How much of the log tail to include.
///
/// Large enough to span a session's setup and its failure, small enough that a
/// human will actually read it before pressing send — which is the point of
/// showing it to them. The interesting part of a Nova failure is always at the
/// end; the beginning is boot noise.
const LOG_TAIL_BYTES: u64 = 256 * 1024;

/// Everything a report carries.
pub struct Bundle {
    pub directory: PathBuf,
    pub description: String,
    /// The issue body. Whether a frame was captured is stated *in here* rather
    /// than carried as a separate field — an idle host has no frame, which is
    /// ordinary, and the report has to say so either way.
    pub summary: String,
}

/// Ask the question, gather the evidence, and hand it off.
///
/// Runs on its own thread: the frame capture waits for the encoder, the
/// PowerShell dialogs block, and the tray's message pump must keep running
/// through all of it. Called from the tray.
pub fn report_issue() -> Result<String, String> {
    // Armed FIRST, before the dialog. The frame that matters is the one on
    // screen when the user decided something was wrong — and by the time they
    // have typed a sentence, the screen has moved on. Arming before the prompt
    // means the captured frame is from the moment of the complaint rather than
    // the moment of the submission.
    crate::encoder::arm_frame_snapshot();

    let Some(description) = prompt_for_description() else {
        // Cancelled. Drop the armed capture rather than leaving a picture of
        // somebody's desktop sitting in memory for a report they declined.
        crate::encoder::reset_frame_snapshot();
        return Ok("Cancelled".into());
    };

    let bundle = collect(&description)?;
    write_bundle(&bundle)?;
    submit(&bundle)
}

// ── The question ────────────────────────────────────────────────────────────

/// One field, one question.
///
/// Deliberately not a form. Every extra field is a field a person abandons the
/// report at, and none of the ones a form would ask (resolution, codec, GPU,
/// bitrate) need asking — Nova already knows all of them. The only thing it
/// cannot know is what the user was doing.
fn prompt_for_description() -> Option<String> {
    let output = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "Add-Type -AssemblyName Microsoft.VisualBasic; \
             $what = [Microsoft.VisualBasic.Interaction]::InputBox(\
                'What were you doing when the issue occurred?', \
                'Nova — Report an issue', ''); \
             if ($what -eq '') { exit 1 }; \
             Write-Output $what",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

// ── Gathering ───────────────────────────────────────────────────────────────

fn collect(description: &str) -> Result<Bundle, String> {
    let directory = debug::log_path()
        .parent()
        .unwrap_or(Path::new("."))
        .join("bugreports")
        .join(timestamp());
    std::fs::create_dir_all(&directory)
        .map_err(|e| format!("could not create the report directory: {e}"))?;

    // The frame the encoder kept when the dialog opened. A failure here is never
    // fatal to the report: an idle host has no frame, and "no picture attached"
    // is a far better outcome than "no report".
    let frame_path = directory.join("last-frame.png");
    let frame = match crate::encoder::take_frame_snapshot_png(&frame_path) {
        Ok(true) => Some(frame_path),
        Ok(false) => None,
        Err(code) => {
            println!("⚠️  Bug report: the frame snapshot failed (code {code}) — continuing without it");
            None
        }
    };
    // Whether it was used or not, nothing stays armed and nothing stays held.
    crate::encoder::reset_frame_snapshot();

    // Built after the capture, so the "Attached" section states what is actually
    // there rather than what was hoped for. A report that promises a frame it
    // does not carry sends its reader looking for a missing file.
    let summary = build_summary(description, frame.as_deref());
    Ok(Bundle { directory, description: description.to_string(), summary })
}

/// The human-readable half — what goes in the issue body.
fn build_summary(description: &str, frame: Option<&Path>) -> String {
    let stats = crate::stats::snapshot();
    let (host, os, cpu, gpu, ram) = hardware();

    let mut out = String::new();
    out.push_str("### What happened\n\n");
    out.push_str(&redact(description));
    out.push_str("\n\n### Host\n\n");
    out.push_str(&format!("| | |\n|---|---|\n"));
    out.push_str(&format!("| Nova | {} |\n", env!("CARGO_PKG_VERSION")));
    out.push_str(&format!("| Machine | {host} |\n"));
    out.push_str(&format!("| OS | {os} |\n"));
    out.push_str(&format!("| CPU | {cpu} |\n"));
    out.push_str(&format!("| GPU | {gpu} |\n"));
    out.push_str(&format!("| RAM | {ram} |\n"));

    out.push_str("\n### Stream at the time of the report\n\n");
    if stats.streaming {
        out.push_str(&format!("| | |\n|---|---|\n"));
        out.push_str(&format!("| Resolution | {} |\n", stats.resolution_text()));
        out.push_str(&format!("| Frame rate | {} |\n", stats.fps_text()));
        out.push_str(&format!("| Codec | {}{} |\n", stats.codec, if stats.hdr { " HDR" } else { "" }));
        out.push_str(&format!(
            "| Bitrate | {} of {} negotiated |\n",
            crate::stats::Snapshot::rate_text(stats.measured_kbps),
            crate::stats::Snapshot::rate_text(stats.ceiling_kbps),
        ));
        out.push_str(&format!(
            "| Rate control | targeting {} |\n",
            crate::stats::Snapshot::rate_text(stats.target_kbps)
        ));
    } else {
        // Worth stating rather than omitting: "nothing was streaming" is itself
        // a diagnosis for a whole class of reports.
        out.push_str("Nothing was streaming when this report was filed.\n");
    }

    if let Some(path) = crate::recording::current_path() {
        // Named rather than merely flagged: a report that says a recording
        // exists and does not say where is a report that costs a round trip to
        // ask.
        out.push_str(&format!(
            "\nA local recording was in progress: `{}`\n",
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
    }

    out.push_str("\n### Attached\n\n");
    out.push_str("- `nova.log` — Worker: capture, encode, input\n");
    out.push_str("- `nova-service.log` — Master: networking, pairing, sessions\n");
    match frame {
        Some(path) => out.push_str(&format!(
            "- `{}` — the frame the encoder produced when this was filed\n",
            path.file_name().unwrap_or_default().to_string_lossy()
        )),
        // Stated, not omitted. "No frame" distinguishes an idle host from a
        // capture that failed, and a reader who is told nothing assumes the
        // latter.
        None => out.push_str(
            "- no frame — nothing was being encoded when this was filed\n",
        ),
    }
    out.push_str("\n<sub>Addresses, machine and account names, and certificate fingerprints \
                  have been redacted. Private LAN addresses are kept — they are what makes a \
                  topology readable and they identify nobody.</sub>\n");
    out
}

/// `(machine, os, cpu, gpu, ram)`.
///
/// One PowerShell invocation for all five: each `Get-CimInstance` costs a WMI
/// round trip of a few hundred milliseconds, and five separate processes would
/// make the button feel broken.
fn hardware() -> (String, String, String, String, String) {
    let script = "\
        $os  = Get-CimInstance Win32_OperatingSystem; \
        $cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name; \
        $gpu = (Get-CimInstance Win32_VideoController | \
                Where-Object { $_.AdapterRAM -ne $null -or $_.Name -match 'NVIDIA|AMD|Intel' } | \
                ForEach-Object { \"$($_.Name) [$($_.DriverVersion)]\" }) -join '; '; \
        $ram = [math]::Round($os.TotalVisibleMemorySize / 1MB, 1); \
        Write-Output \"$env:COMPUTERNAME|$($os.Caption) $($os.BuildNumber)|$cpu|$gpu|$ram GB\"";

    let fallback = || "unavailable".to_string();
    let Ok(output) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", script])
        .output()
    else {
        return (fallback(), fallback(), fallback(), fallback(), fallback());
    };

    let raw = String::from_utf8_lossy(&output.stdout);
    let mut parts = raw.trim().split('|');
    let mut next = || parts.next().unwrap_or("unavailable").trim().to_string();
    let machine = next();
    let os = next();
    let cpu = next();
    let gpu = next();
    let ram = next();
    // The machine name is redacted in the body but kept here as a marker, so a
    // reader can tell two reports apart without learning whose desk they came
    // from.
    (redact(&machine), os, cpu, gpu, ram)
}

// ── Redaction ───────────────────────────────────────────────────────────────

/// Remove what identifies a person or a network from text that is about to be
/// published.
///
/// What goes, and why:
///
/// - **Public IP addresses.** These locate a household. Nova's logs are full of
///   them — STUN mappings, relay endpoints, punched peers.
/// - **Certificate fingerprints**, truncated to eight characters. Full ones are
///   stable device identifiers across every report a person ever files; eight
///   characters is still enough to tell two devices apart within one report,
///   which is all a reader needs.
/// - **The machine and account names**, which are usually a person's name.
///
/// What stays, deliberately: **private LAN addresses**. `10.0.0.205` identifies
/// nobody, and the shape of a local topology — which interface, which subnet,
/// how many of them — is exactly what makes a networking report readable. This
/// project has one bug (the `0.0.0.0` mDNS advertisement) that was only
/// diagnosable because the log named five local addresses.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < bytes.len() {
        // A run of hex long enough to be a fingerprint.
        if bytes[i].is_ascii_hexdigit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
            let run: String = bytes[start..i].iter().collect();
            if run.len() >= 32 {
                out.push_str(&run[..8]);
                out.push_str("…redacted");
                continue;
            }
            // Not a fingerprint. It may still be the first octet of an address,
            // so hand it to the address test rather than emitting it blind.
            //
            // Only when nothing dotted-numeric precedes it, though. Without that
            // guard a driver version like `566.36.1.2.3` matches from its SECOND
            // component onward — `36.1.2.3` is a perfectly well-formed public
            // address — and the log line comes out as `566.<public-ip>`. Over-
            // redaction is its own failure: it destroys information silently and
            // in a way nobody notices until the report is useless.
            let continues_a_number = start > 0
                && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == '.');
            if continues_a_number {
                out.push_str(&run);
                continue;
            }
            if let Some((len, replacement)) = ipv4_at(&bytes, start) {
                out.push_str(&replacement);
                i = start + len;
                continue;
            }
            out.push_str(&run);
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }

    let mut out = out;
    for (needle, mask) in identity_masks() {
        if !needle.is_empty() {
            out = replace_ignore_case(&out, &needle, mask);
        }
    }
    out
}

/// Machine and account names, with what to replace them with.
fn identity_masks() -> Vec<(String, &'static str)> {
    let mut masks = Vec::new();
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        masks.push((name, "<machine>"));
    }
    if let Ok(name) = std::env::var("USERNAME") {
        masks.push((name, "<user>"));
    }
    // Longest first, so a machine named after its user does not get half-masked.
    masks.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    masks
}

fn replace_ignore_case(haystack: &str, needle: &str, mask: &str) -> String {
    let lower_hay = haystack.to_lowercase();
    let lower_needle = needle.to_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0;
    while let Some(found) = lower_hay[cursor..].find(&lower_needle) {
        let at = cursor + found;
        out.push_str(&haystack[cursor..at]);
        out.push_str(mask);
        cursor = at + needle.len();
    }
    out.push_str(&haystack[cursor..]);
    out
}

/// If a dotted-quad starts at `start`, return its length and what to emit.
///
/// Private, loopback and link-local addresses are returned unchanged — see
/// [`redact`] on why keeping them is the right call.
fn ipv4_at(chars: &[char], start: usize) -> Option<(usize, String)> {
    let mut i = start;
    let mut octets = [0u16; 4];
    for slot in 0..4 {
        if slot > 0 {
            if i >= chars.len() || chars[i] != '.' {
                return None;
            }
            i += 1;
        }
        let digits_start = i;
        while i < chars.len() && chars[i].is_ascii_digit() && i - digits_start < 3 {
            i += 1;
        }
        if i == digits_start {
            return None;
        }
        let value: u16 = chars[digits_start..i].iter().collect::<String>().parse().ok()?;
        if value > 255 {
            return None;
        }
        octets[slot] = value;
    }
    // A longer number follows — this was not an address.
    if i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
        return None;
    }

    let addr = std::net::Ipv4Addr::new(
        octets[0] as u8,
        octets[1] as u8,
        octets[2] as u8,
        octets[3] as u8,
    );
    let text: String = chars[start..i].iter().collect();
    if addr.is_private() || addr.is_loopback() || addr.is_link_local() || addr.is_unspecified() {
        Some((i - start, text))
    } else {
        Some((i - start, "<public-ip>".to_string()))
    }
}

// ── Writing and submitting ──────────────────────────────────────────────────

fn write_bundle(bundle: &Bundle) -> Result<(), String> {
    std::fs::write(bundle.directory.join("report.md"), &bundle.summary)
        .map_err(|e| format!("could not write the report: {e}"))?;

    // Both logs, because a Worker-side symptom is invisible in the Master's log
    // and vice versa — that split is documented and it is exactly the mistake a
    // one-log bundle would keep making.
    for (source, name) in [
        (debug::log_path(), "nova.log"),
        (debug::service_log_path(), "nova-service.log"),
        (debug::input_helper_log_path(), "nova-input.log"),
    ] {
        if let Some(tail) = read_tail(&source, LOG_TAIL_BYTES) {
            let _ = std::fs::write(bundle.directory.join(name), redact(&tail));
        }
    }
    Ok(())
}

/// The last `limit` bytes of a file, as text.
///
/// Reads from the end rather than reading the whole thing: `nova.log` on a box
/// that has been up for a week is tens of megabytes, and loading it to keep the
/// last quarter-megabyte would be a visible pause on the button press.
fn read_tail(path: &Path, limit: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    // Shared read AND write: the process that owns this log still has it open,
    // and an exclusive open would fail — the same collision that once cost this
    // project every shim log line.
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(limit);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::with_capacity(limit as usize);
    file.read_to_end(&mut buf).ok()?;
    // Lossy on purpose: a log is written by several processes and one bad byte
    // must not cost the whole attachment.
    Some(String::from_utf8_lossy(&buf).into_owned())
}


// ── Packaging and handoff ───────────────────────────────────────────────────

/// Zip the bundle and open a prefilled issue in the user's browser.
///
/// **No credential, no server, no upload.** This is the standard open-source
/// shape and it is chosen rather than settled for: Nova ships as an installer,
/// so any token compiled into it is readable by everyone who has a copy and can
/// only be revoked for everyone at once. An intake service would fix that and
/// costs a server the project does not have.
///
/// So the flow is: Nova builds the evidence, the browser opens the issue form
/// already written, and the user attaches the zip and presses submit. The user
/// posts under their own account, sees exactly what they are sharing, and can
/// edit or abandon it. That the last step is manual is the feature — a reporter
/// that uploaded a screenshot of somebody's desktop on their behalf would be
/// doing something they never agreed to.
fn submit(bundle: &Bundle) -> Result<String, String> {
    let cfg = crate::config::NovaConfig::load();
    let repository = cfg.bugreport.repository.trim();
    let repository = if repository.is_empty() { DEFAULT_REPOSITORY } else { repository };

    // The zip is the deliverable. If it cannot be produced the report is still
    // worth filing — the folder is right there and the user can attach its
    // contents — so this reports rather than aborts.
    let archive = match zip_bundle(bundle, &cfg.bugreport.output_dir) {
        Ok(path) => Some(path),
        Err(e) => {
            println!("⚠️  Bug report: could not create the .zip ({e}) — the folder is still there");
            None
        }
    };

    let body = issue_body(bundle, archive.as_deref());
    let url = issue_url(
        repository,
        &format!("[bug] {}", first_line(&bundle.description)),
        &body,
    );

    // A durable, double-clickable way back to this exact prefilled form.
    //
    // The browser launch below can succeed and still not be SEEN: while Nova is
    // streaming headless, the desktop lives on the virtual display, so the window
    // opens on the screen being sent to the client rather than on the physical
    // monitor the user is sitting at. That is inherent to how Nova streams — it
    // is not a launch failure and there is nothing to "fix" about it — but it
    // does mean a fire-and-forget launch is not good enough on its own.
    //
    // So the shortcut sits next to the zip, and the URL goes in the log. Both
    // survive the window opening somewhere the user is not looking.
    let shortcut = archive
        .as_ref()
        .and_then(|zip| zip.parent())
        .map(|dir| dir.join("Open Nova bug report.url"));
    if let Some(shortcut) = &shortcut {
        // Internet Shortcut format — an INI that Explorer opens with the
        // default browser, unelevated, via the user's own shell.
        let _ = std::fs::write(shortcut, format!("[InternetShortcut]\r\nURL={url}\r\n"));
    }
    println!("🐞 Bug report: issue URL is {url}");

    // Reveal the zip BEFORE the browser, so the file the form asks for is
    // already selected in an Explorer window behind it. Reversing these leaves
    // the user on a page telling them to drag a file they now have to go and
    // find.
    match &archive {
        Some(path) => reveal_in_explorer(path),
        None => reveal_in_explorer(&bundle.directory),
    }

    // A failed launch is reported, not swallowed. The report is already on disk
    // and the shortcut already written, so this is a degraded success rather
    // than a failure — saying "the form is open" when it is not is the part that
    // would waste someone's time.
    let opened = open_url(&url).is_ok();

    let where_ = archive.as_ref().map(|p| p.display().to_string());
    Ok(match (where_, opened) {
        (Some(zip), true) => format!("Saved {zip} — the issue form is open in your browser"),
        (Some(zip), false) => format!(
            "Saved {zip} — could not open the browser; use \"Open Nova bug report.url\" beside it"
        ),
        (None, true) => format!(
            "Saved {} (no .zip — attach the files by hand) — the issue form is open",
            bundle.directory.display()
        ),
        (None, false) => format!(
            "Saved {} — no .zip and no browser; the issue URL is in nova.log",
            bundle.directory.display()
        ),
    })
}

/// The largest URL Windows will actually ACT on.
///
/// `INTERNET_MAX_URL_LENGTH` is 2083, and it is not advisory: past it the shell
/// silently declines. Measured 2026-08-24 — a 2347-character `URL=` line in the
/// `.url` shortcut produced a file Explorer parsed happily (the tooltip showed
/// the full address) and clicking it did nothing at all. The same limit is why
/// the browser never opened: the earlier launch test used the bare repo URL,
/// which is 33 characters, rather than the real prefilled one.
///
/// The budget is spent below 2083 so that neither the shortcut nor the direct
/// launch is anywhere near the edge.
const MAX_SHELL_URL: usize = 1900;

/// Build the issue URL, trimming the body until the whole thing fits.
///
/// Trimming the BODY rather than refusing: an issue form that opens with a
/// slightly shortened description is worth far more than one that does not open.
fn issue_url(repository: &str, title: &str, body: &str) -> String {
    let build = |body: &str| {
        format!(
            "https://github.com/{repository}/issues/new?title={}&body={}",
            urlencode(title),
            urlencode(body),
        )
    };

    let full = build(body);
    if full.len() <= MAX_SHELL_URL {
        return full;
    }

    // Percent-encoding expands unpredictably (1–9 bytes per character), so the
    // fit is found by measurement rather than arithmetic. Shrinking by a
    // proportion of the overshoot converges in a handful of passes on any input.
    let mut keep = body.len();
    loop {
        let overshoot = build(&body[..body.floor_char_boundary(keep)]).len();
        if overshoot <= MAX_SHELL_URL || keep == 0 {
            break;
        }
        keep = keep.saturating_sub((keep / 4).max(64));
    }
    let trimmed = &body[..body.floor_char_boundary(keep)];
    build(&format!("{trimmed}\n\n_(trimmed — the full report is in the attached zip)_"))
}

/// The Markdown the issue form opens already containing.
///
/// **Deliberately short.** The first version put the whole diagnostic summary in
/// here — host table, stream table, attachment list — which pushed the URL past
/// the shell's limit and stopped it opening at all. It was the wrong content for
/// this box anyway: everything in that summary is already in `report.md` inside
/// the zip, so putting it in the URL duplicated it into the one place with a hard
/// size cap, and greeted the reporter with a wall of generated text above the
/// field they actually need to fill in.
///
/// What stays is what only a human can supply, and the instruction that makes
/// the attachment happen.
fn issue_body(bundle: &Bundle, archive: Option<&Path>) -> String {
    let attach = match archive {
        Some(path) => format!(
            "**Please drag `{}` into this box before submitting.**\n\n\
             It is on your Desktop, and the Explorer window Nova just opened has it selected. \
             It contains the host logs, this machine's specifications, and a snapshot of the \
             last frame the encoder produced.",
            path.file_name().unwrap_or_default().to_string_lossy()
        ),
        None => "**Please attach the report folder Nova just opened.** The `.zip` could not be \
                 created automatically, so the individual files need attaching instead."
            .to_string(),
    };

    // One line of host context, not the whole table. Enough for triage to route
    // the issue without opening the attachment; everything else is in there.
    let stats = crate::stats::snapshot();
    let context = if stats.streaming {
        format!(
            "Nova {} · {} {} @ {}",
            env!("CARGO_PKG_VERSION"),
            stats.resolution_text(),
            stats.codec,
            stats.target_fps,
        )
    } else {
        format!("Nova {} · not streaming", env!("CARGO_PKG_VERSION"))
    };

    format!(
        "## What went wrong\n\n\
         {description}\n\n\
         ## What I expected instead\n\n\
         _Replace this line._\n\n\
         ## Steps to reproduce\n\n\
         1. _Replace this line._\n\
         2. \n\
         3. \n\n\
         ## Diagnostics\n\n\
         {attach}\n\n\
         <sub>{context}</sub>",
        description = bundle.description.trim(),
        attach = attach,
        context = context,
    )
}

/// Compress the report directory into a single `.zip`.
///
/// Uses PowerShell's `Compress-Archive` rather than a zip crate. The project's
/// stated goal is a minimal portable exe and this is a button a human presses
/// once; the same reasoning already puts the pairing dialogs and the ViGEmBus
/// download on PowerShell. `Compress-Archive` ships with every supported
/// Windows and deflates, which matters here — a log tail compresses roughly ten
/// to one, and GitHub has an attachment size limit.
///
/// Returns the archive path.
fn zip_bundle(bundle: &Bundle, configured_dir: &str) -> Result<PathBuf, String> {
    let dir = output_dir(configured_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    let name = bundle
        .directory
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "report".into());
    let archive = dir.join(format!("nova-report-{name}.zip"));
    // Compress-Archive refuses to overwrite without -Force, and -Force on a
    // path that already exists is the only way a second report in the same
    // second does not fail.
    let _ = std::fs::remove_file(&archive);

    let script = format!(
        "$ErrorActionPreference='Stop'; \
         Compress-Archive -Path '{}\\*' -DestinationPath '{}' -CompressionLevel Optimal -Force",
        escape_ps(&bundle.directory.to_string_lossy()),
        escape_ps(&archive.to_string_lossy()),
    );

    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .output()
        .map_err(|e| format!("could not run Compress-Archive: {e}"))?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    // Trust the exit code only as far as the file: a zip that does not exist is
    // a zip the user cannot attach, whatever PowerShell reported.
    if !archive.exists() {
        return Err("Compress-Archive reported success but wrote no file".into());
    }
    Ok(archive)
}

/// Where the finished `.zip` goes.
///
/// The Desktop by default, because the next thing that happens is a drag into a
/// browser and the Desktop is the one folder every user can find during that.
/// A configured `[bugreport] output_dir` wins.
///
/// **`%USERPROFILE%\Desktop` is the wrong way to ask.** OneDrive's Known Folder
/// Move redirects the Desktop, and on a redirected machine that path does not
/// exist at all — so the first version fell straight through to its fallback and
/// wrote the zip beside the exe, in Program Files, which is precisely where a
/// user will not look. Measured on the dev box 2026-08-24: `USERPROFILE` is
/// `C:\Users\bobby`, `C:\Users\bobby\Desktop` does not exist, and the real
/// Desktop is `C:\Users\bobby\OneDrive\Desktop`.
///
/// [`SHGetKnownFolderPath`] is the API that answers the question actually being
/// asked — "where does the shell put things on this user's Desktop" — and it
/// follows the redirection. The environment variable is kept only as a fallback
/// for the case where the shell API fails.
fn output_dir(configured: &str) -> PathBuf {
    let configured = configured.trim();
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    if let Some(desktop) = known_desktop() {
        if desktop.is_dir() {
            return desktop;
        }
    }
    // Legacy shape, for a shell API that refused. Still checked for existence:
    // handing back a path that is not there would put us back where we started.
    if let Ok(profile) = std::env::var("USERPROFILE") {
        let desktop = PathBuf::from(profile).join("Desktop");
        if desktop.is_dir() {
            return desktop;
        }
    }
    // No Desktop at all — a Worker respawned under the SYSTEM fallback has no
    // user profile. Beside the exe is not a good answer, but it is a real one,
    // and the log line names it.
    debug::log_path().parent().unwrap_or(Path::new(".")).join("bugreports")
}

/// The user's Desktop as the shell resolves it, honouring redirection.
fn known_desktop() -> Option<PathBuf> {
    use windows::Win32::UI::Shell::{FOLDERID_Desktop, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        // KF_FLAG_DEFAULT, not KF_FLAG_CREATE: if the folder genuinely is not
        // there, that is something to fall back from, not to materialise inside
        // somebody's profile as a side effect of filing a bug report.
        let raw = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).ok()?;
        let path = PathBuf::from(raw.to_string().ok()?);
        // The returned PWSTR is CoTaskMem and ours to free.
        windows::Win32::System::Com::CoTaskMemFree(Some(raw.0 as *const _));
        Some(path)
    }
}

/// Open Explorer with `path` selected, so a drag-and-drop is one motion away.
///
/// `/select,` needs the argument as ONE token including the comma; splitting it
/// opens the user's Documents folder instead, which is the sort of failure
/// nobody reports and everybody works around.
fn reveal_in_explorer(path: &Path) {
    let selected = path.is_file();
    let mut cmd = std::process::Command::new("explorer");
    if selected {
        cmd.arg(format!("/select,{}", path.display()));
    } else {
        cmd.arg(path);
    }
    // Explorer's exit code is meaningless (it returns 1 on success routinely),
    // so nothing is checked here. A failure to open a window must not fail a
    // report that has already been written to disk.
    let _ = cmd.spawn();
}
/// Escape for a PowerShell single-quoted string, where `'` is doubled.
fn escape_ps(text: &str) -> String {
    text.replace('\'', "''")
}

fn open_url(url: &str) -> Result<(), String> {
    // `explorer` rather than `start`: `start` is a cmd builtin, so it needs a
    // shell, and a shell needs the URL quoted — which is one more layer of
    // escaping around a string that already contains percent-encoding.
    std::process::Command::new("explorer")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("could not open the browser: {e}"))
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").trim().chars().take(120).collect()
}

fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

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
    fn private_addresses_survive_and_public_ones_do_not() {
        // The whole point of the split: a LAN topology stays readable while a
        // household stays unlocated.
        let text = "punched 10.0.0.205:47998 -> 203.0.113.44:51820 via 192.168.1.1";
        let out = redact(text);
        assert!(out.contains("10.0.0.205"), "{out}");
        assert!(out.contains("192.168.1.1"), "{out}");
        assert!(!out.contains("203.0.113.44"), "{out}");
        assert!(out.contains("<public-ip>"), "{out}");
    }

    #[test]
    fn a_fingerprint_keeps_just_enough_to_tell_devices_apart() {
        let fp = "a".repeat(64);
        let out = redact(&format!("client fp {fp} paired"));
        assert!(out.contains("aaaaaaaa…redacted"), "{out}");
        assert!(!out.contains(&fp), "the full fingerprint must not survive");
    }

    #[test]
    fn ordinary_hex_and_numbers_are_left_alone() {
        // Over-redaction is its own failure: an error code or a port turned into
        // a placeholder makes the log useless in a way that is hard to notice.
        let text = "HRESULT=0x887A0001 port 47998 frame 12345";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn a_version_string_is_not_mistaken_for_an_address() {
        let text = "driver 566.36.1.2.3 loaded";
        let out = redact(text);
        assert!(out.contains("566.36.1.2.3"), "{out}");
    }

    #[test]
    fn powershell_quotes_in_a_url_cannot_break_out_of_their_string() {
        assert_eq!(escape_ps("it's"), "it''s");
    }

    fn a_bundle(description: &str) -> Bundle {
        Bundle {
            directory: PathBuf::from(r"C:\x\bugreports\20260824-101500"),
            description: description.into(),
            summary: "### Host\n\n| Nova | 0.1.0 |\n".into(),
        }
    }

    /// The template has to tell the user what to do with the file, or the whole
    /// point of packaging it is lost — an issue with no attachment is the
    /// unactionable report this feature exists to replace.
    #[test]
    fn the_issue_body_names_the_zip_and_asks_for_it() {
        let zip = PathBuf::from(r"C:\Users\x\Desktop\nova-report-20260824-101500.zip");
        let body = issue_body(&a_bundle("the picture froze"), Some(&zip));
        assert!(body.contains("nova-report-20260824-101500.zip"), "{body}");
        assert!(body.contains("drag"), "the body must ask for the attachment: {body}");
        assert!(body.contains("the picture froze"), "the user's words must survive");
        assert!(body.contains("## Steps to reproduce"), "template sections missing");
    }

    /// A failed zip must not produce a template that points at a file which does
    /// not exist — that sends the user hunting for something Nova never wrote.
    #[test]
    fn without_a_zip_the_body_asks_for_the_folder_instead() {
        let body = issue_body(&a_bundle("no picture"), None);
        assert!(body.contains("folder"), "{body}");
        // Explaining that the zip could not be made is fine and necessary. What
        // must never appear is a specific FILENAME, which would send the user
        // hunting for something Nova never wrote.
        assert!(
            !body.contains("nova-report-"),
            "must not name a zip that was never created: {body}"
        );
    }

    #[test]
    fn a_configured_output_directory_wins_over_the_desktop() {
        assert_eq!(output_dir(r"D:\reports"), PathBuf::from(r"D:\reports"));
        // Blank falls through to the Desktop, or to the report folder when there
        // is no profile — either way it must produce SOMEWHERE, never a panic.
        assert!(output_dir("  ").is_absolute() || output_dir("  ").components().count() > 0);
    }

    /// The URL is built from this, so an empty owner would produce
    /// `github.com//issues/new` — a 404 at the exact moment a user is trying to
    /// help.
    /// The limit that made the shortcut inert and stopped the browser opening.
    ///
    /// Explorer parses a `.url` whose `URL=` line is any length — the tooltip
    /// showed all 2347 characters — but the shell refuses to LAUNCH past
    /// `INTERNET_MAX_URL_LENGTH` (2083), silently.
    #[test]
    fn the_issue_url_stays_launchable_however_long_the_report_is() {
        let huge = "x".repeat(20_000);
        let url = issue_url(DEFAULT_REPOSITORY, "[bug] something broke", &huge);
        assert!(
            url.len() <= MAX_SHELL_URL,
            "URL is {} chars — Windows will not launch it",
            url.len()
        );
        assert!(url.len() < 2083, "must stay under INTERNET_MAX_URL_LENGTH");
        assert!(url.starts_with("https://github.com/"), "still a real issue URL");
        assert!(url.contains("trimmed"), "a trimmed body must say so");
    }

    /// A body that already fits must be passed through untouched — trimming a
    /// short report would lose the user's words for nothing.
    #[test]
    fn a_short_report_is_not_trimmed() {
        let url = issue_url(DEFAULT_REPOSITORY, "[bug] x", "the screen went black");
        assert!(url.len() <= MAX_SHELL_URL);
        assert!(!url.contains("trimmed"));
    }

    /// The real shape, at the size it actually occurs — this is what regressed.
    #[test]
    fn a_realistic_report_fits_with_room_to_spare() {
        let zip = PathBuf::from(r"C:\Users\x\Desktop\nova-report-20260824-101500.zip");
        let bundle = a_bundle(
            "I disabled Wi-Fi mid-stream and the picture froze, then the app said Failed",
        );
        let body = issue_body(&bundle, Some(&zip));
        let url = issue_url(DEFAULT_REPOSITORY, "[bug] wifi handover froze", &body);
        assert!(url.len() <= MAX_SHELL_URL, "realistic report produced {} chars", url.len());
        assert!(!url.contains("trimmed"), "a normal report should never need trimming");
    }

    #[test]
    fn the_default_repository_is_a_real_owner_and_repo() {
        let (owner, repo) = DEFAULT_REPOSITORY.split_once('/').expect("owner/repo");
        assert!(!owner.is_empty() && !repo.is_empty());
    }

    #[test]
    fn the_issue_title_is_one_bounded_line() {
        let long = format!("{}\nsecond line", "x".repeat(400));
        let title = first_line(&long);
        assert_eq!(title.len(), 120);
        assert!(!title.contains('\n'));
    }
}
