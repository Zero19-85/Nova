//! The hybrid bug reporter: one question, and everything the answer needs.
//!
//! "It went wrong" is the least actionable report Nova receives, and it is
//! almost the only kind that arrives — because the things that would make it
//! actionable are a log file the user does not know exists, a GPU model they
//! have no reason to mention, and a frame that never left the encoder. This
//! module asks the one question a human can answer and assembles the rest
//! silently.
//!
//! ## The credential problem, stated plainly
//!
//! The obvious implementation posts straight to the GitHub Issues API with a
//! token compiled into the binary. **That token is not a secret.** Nova ships as
//! an installer; anyone who has it can extract the token and post as whatever
//! account it belongs to, and revoking it breaks every copy already deployed. It
//! is a credential handed to everyone, with write access to the project's issue
//! tracker.
//!
//! So there are three submission modes, and the DEFAULT is the one that needs no
//! credential at all:
//!
//! 1. **Browser handoff (default).** The bundle is written to disk and GitHub's
//!    prefilled-issue URL is opened. The user posts under their own account,
//!    having read what they are sending, and attaches the bundle themselves.
//!    Nothing is uploaded by Nova, and there is no secret to leak.
//! 2. **Intake endpoint** (`[bugreport] endpoint`). A URL the project runs that
//!    holds the GitHub credential server-side. This is the right answer for a
//!    project that wants one-press reporting; the credential lives somewhere it
//!    can be rotated.
//! 3. **Direct token** (`[bugreport] github_token`). For a maintainer running
//!    their own fork against their own tracker. Never shipped set.
//!
//! ## Consent is not a formality here
//!
//! The bundle contains a log that names the machine, the user, every paired
//! device, and every address the host has spoken to. [`redact`] removes the
//! worst of it, and the dialog shows the user the bundle before anything leaves
//! the machine. A reporter that quietly uploaded a desktop screenshot and a
//! network trace would be doing something the user did not agree to, however
//! good the intent.
//!
//! ## Why PowerShell for the dialogs and the upload
//!
//! Precedent, and footprint. `tray.rs` already drives its pairing dialogs
//! through `Microsoft.VisualBasic.Interaction`, and the ViGEmBus bootstrap
//! already downloads through `Invoke-WebRequest`. Adding an HTTPS client to the
//! host binary would mean a TLS stack and a root-certificate store for one
//! button, in a project whose stated goal is a minimal portable exe.

use std::path::{Path, PathBuf};

use crate::debug;

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

/// Hand the finished bundle off, by whichever route is configured.
fn submit(bundle: &Bundle) -> Result<String, String> {
    let cfg = crate::config::NovaConfig::load();
    let endpoint = cfg.bugreport.endpoint.trim();
    let token = cfg.bugreport.github_token.trim();

    if !endpoint.is_empty() {
        return post_to_endpoint(endpoint, bundle);
    }
    if !token.is_empty() {
        return post_to_github(&cfg.bugreport.repository, token, bundle);
    }
    open_prefilled_issue(&cfg.bugreport.repository, bundle)
}

/// The default: let the user post it themselves.
///
/// GitHub's `/issues/new` accepts a URL-encoded title and body, so the issue
/// opens already written. The user reads it, sees exactly what is being shared,
/// and attaches the bundle directory Nova just opened for them.
///
/// The body is truncated because a URL has a practical length limit and browsers
/// silently mangle long ones — so what goes in the link is the summary, and the
/// logs stay as files the user drags in.
fn open_prefilled_issue(repository: &str, bundle: &Bundle) -> Result<String, String> {
    const URL_BODY_LIMIT: usize = 4000;
    let body = if bundle.summary.len() > URL_BODY_LIMIT {
        format!(
            "{}\n\n_(truncated — the full report and logs are in the folder Nova opened)_",
            &bundle.summary[..bundle.summary.floor_char_boundary(URL_BODY_LIMIT)]
        )
    } else {
        bundle.summary.clone()
    };

    let title = first_line(&bundle.description);
    let url = format!(
        "https://github.com/{repository}/issues/new?title={}&body={}",
        urlencode(&format!("[report] {title}")),
        urlencode(&body),
    );

    // The folder first, so the attachments are already in front of the user when
    // the browser lands on the issue form.
    let _ = std::process::Command::new("explorer")
        .arg(&bundle.directory)
        .spawn();
    open_url(&url)?;
    Ok(format!(
        "Report prepared in {} — the issue form is open in your browser",
        bundle.directory.display()
    ))
}

/// Post to a project-run intake service, which holds the GitHub credential.
fn post_to_endpoint(endpoint: &str, bundle: &Bundle) -> Result<String, String> {
    let payload = serde_json::json!({
        "nova_version": env!("CARGO_PKG_VERSION"),
        "title": first_line(&bundle.description),
        "body": bundle.summary,
    });
    let response = powershell_post(endpoint, &payload.to_string(), &[])?;
    Ok(format!("Report submitted ({})", first_line(&response)))
}

/// Post directly to the GitHub Issues API with a configured token.
///
/// Only reachable when an operator has put their own token in `nova.toml`. Never
/// shipped set — see the credential note at the top of this module.
fn post_to_github(repository: &str, token: &str, bundle: &Bundle) -> Result<String, String> {
    let payload = serde_json::json!({
        "title": format!("[report] {}", first_line(&bundle.description)),
        "body": bundle.summary,
    });
    let url = format!("https://api.github.com/repos/{repository}/issues");
    let response = powershell_post(
        &url,
        &payload.to_string(),
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Accept", "application/vnd.github+json"),
            ("X-GitHub-Api-Version", "2022-11-28"),
            ("User-Agent", "nova-server"),
        ],
    )?;
    Ok(format!("Issue filed ({})", first_line(&response)))
}

/// One HTTPS POST, through PowerShell.
///
/// See the module header for why this is not a Rust HTTP client. The body is
/// passed on **stdin** rather than interpolated into the command line: it
/// contains a user's free text and a log, and a quoted-string command line is a
/// shell-injection surface plus a length limit.
fn powershell_post(url: &str, body: &str, headers: &[(&str, &str)]) -> Result<String, String> {
    use std::io::Write;

    let header_literal = headers
        .iter()
        .map(|(k, v)| format!("'{}' = '{}'", escape_ps(k), escape_ps(v)))
        .collect::<Vec<_>>()
        .join("; ");

    let script = format!(
        "$body = [Console]::In.ReadToEnd(); \
         $headers = @{{ {header_literal} }}; \
         try {{ \
            $r = Invoke-RestMethod -Uri '{}' -Method Post -Body $body \
                 -ContentType 'application/json' -Headers $headers -TimeoutSec 30; \
            if ($r.html_url) {{ Write-Output $r.html_url }} else {{ Write-Output 'accepted' }} \
         }} catch {{ Write-Error $_.Exception.Message; exit 1 }}",
        escape_ps(url)
    );

    let mut child = std::process::Command::new("powershell")
        .args(["-NoProfile", "-WindowStyle", "Hidden", "-Command", &script])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run the upload: {e}"))?;

    child
        .stdin
        .as_mut()
        .ok_or("the upload process refused its input")?
        .write_all(body.as_bytes())
        .map_err(|e| format!("could not send the report: {e}"))?;

    let output = child.wait_with_output().map_err(|e| format!("upload failed: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "the report could not be submitted: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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

    #[test]
    fn the_issue_title_is_one_bounded_line() {
        let long = format!("{}\nsecond line", "x".repeat(400));
        let title = first_line(&long);
        assert_eq!(title.len(), 120);
        assert!(!title.contains('\n'));
    }
}
