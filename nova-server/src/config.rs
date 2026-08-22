use std::path::PathBuf;
use serde::Deserialize;

// ── Top-level ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NovaConfig {
    pub stream:  StreamConfig,
    pub audio:   AudioConfig,
    pub network: NetworkConfig,
    pub hdr:     HdrConfig,
    pub echo:    EchoConfig,
}

// ── Sub-tables ────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct StreamConfig {
    /// VDD boot width — overridden per-session by Moonlight negotiation.
    pub width:        i32,
    /// VDD boot height — overridden per-session by Moonlight negotiation.
    pub height:       i32,
    /// Default encoder bitrate (Kbps); client negotiation takes precedence.
    pub bitrate_kbps: i32,
    /// Default frame rate; client negotiation takes precedence.
    pub fps:          u32,
    /// Startup codec: "h264" | "hevc" | "av1"
    pub codec:        String,
    /// When true, Nova enables HDR10/HEVC-Main10 per-session even if
    /// VirtualDisplay::is_advanced_color_supported() returns false — useful
    /// when HDRPlus=true is set in vdd_settings.xml but the CCD query is slow
    /// to reflect the new capability after a devnode cycle.
    pub enable_hdr:            bool,
    /// Route the launching apps (Steam, Xbox, RetroArch, Virtual Desktop)
    /// through the Virtual Display Driver regardless of which app ID the
    /// client asked for. When false, the VDD is restricted to those same four
    /// by ID — which today is the same set, so the flag is close to inert.
    ///
    /// **App 1 (Desktop) is never affected either way**: it mirrors the
    /// physical primary, which is the whole point of that app. See
    /// [`crate::app_launcher::uses_virtual_display`] for why that exemption
    /// outranks this flag.
    ///
    /// Default true.
    pub headless_for_all_apps: bool,
    /// Seconds a **detached** session is held before Nova tears it down.
    ///
    /// A client that vanishes without saying goodbye (network drop, app
    /// backgrounded, phone in a pocket) does not end its session: the encoder
    /// stops and transmission stops, but the virtual monitor, the desktop
    /// arrangement and whatever is running on it are all left exactly as they
    /// were, so a reconnect inside this window resumes instantly instead of
    /// paying a full display cycle. When it expires, Nova tears down as if
    /// `/cancel` had been sent and the physical monitor comes back.
    ///
    /// 0 disables the timer — a detached session is then held until an explicit
    /// end or a restart.
    ///
    /// Not a timeout on the *stream*: an explicit "End Stream" (from the tray,
    /// the client, or `/cancel`) bypasses this entirely and tears down at once.
    pub detach_grace_secs: Option<u32>,

    /// Deprecated name for [`Self::detach_grace_secs`], still honoured so an
    /// existing `nova.toml` keeps working. Read only through
    /// [`Self::detach_grace`].
    pub idle_teardown_secs: Option<u32>,
}

/// Default for [`StreamConfig::detach_grace_secs`] — 10 minutes.
///
/// Long enough to cover the cases that motivated it (a phone that lost signal, a
/// laptop that slept, a client app backgrounded while its user does something
/// else) and short enough that a genuinely abandoned session returns the
/// operator's monitor within one coffee break.
pub const DEFAULT_DETACH_GRACE_SECS: u32 = 600;

impl StreamConfig {
    /// How long to hold a detached session, in seconds. 0 = hold indefinitely.
    ///
    /// Resolves the current name against the deprecated one: whichever the
    /// operator actually wrote wins, the new name wins if both are present, and
    /// an absent setting means [`DEFAULT_DETACH_GRACE_SECS`]. Distinguishing
    /// "absent" from "explicitly set to the default value" is the whole reason
    /// both fields are `Option` — an upgraded install that had tuned
    /// `idle_teardown_secs` must keep its tuning rather than silently adopting
    /// the new default.
    pub fn detach_grace(&self) -> u32 {
        self.detach_grace_secs
            .or(self.idle_teardown_secs)
            .unwrap_or(DEFAULT_DETACH_GRACE_SECS)
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Friendly name or GUID of the Windows audio render endpoint to use as
    /// the default during streaming. Empty string = Windows system default.
    /// Applied at session start in audio.rs (future work — logged for now).
    pub endpoint_override: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NetworkConfig {
    /// Reed-Solomon FEC parity shards as a percentage of data shards.
    /// 0 disables FEC entirely (useful for LAN-only installs with zero loss).
    ///
    /// This is pure overhead ON TOP of the negotiated video bitrate, so it is
    /// the cheapest headroom in the whole pipeline: at the 90 Mbps a 4K120
    /// session negotiates, 20% parity was adding ~20 Mbps and pushing the wire
    /// rate to ~111 Mbps — enough to saturate the link and starve the ENet
    /// control channel until it timed out (live 2026-08-07).
    ///
    /// **10% since 2026-08-20, and the arithmetic is why it is not a relapse.**
    /// The failure above was 20% of 90 Mbps = ~20 Mbps of parity on a link that
    /// could not carry it. A 1080p120 Echo session runs ~43 Mbps, where 10% is
    /// ~4 Mbps and lands at ~47 — nowhere near the earlier cliff. What it buys
    /// is the case the old comment already anticipated: on congested WiFi a
    /// dropped packet in a P-frame smears a region that an infinite GOP never
    /// repairs until the intra-refresh sweep reaches it, up to 2.5 s later at
    /// 120 fps. Live 2026-08-20, that showed as ghost mouse cursors trailing
    /// behind the real one, and it vanished when the bitrate was lowered.
    ///
    /// Still a per-link setting rather than a universal answer. On a wired LAN
    /// 5% remains ample and this is 5% of a bitrate spent on nothing; on a link
    /// bad enough that 10% does not cover it, lowering the bitrate is the
    /// better lever, because parity that does not fit is loss with extra steps.
    pub fec_percentage: u32,

    /// Bandwidth held back from the video encoder for the audio pipelines, in
    /// Kbps. Subtracted from the session's ceiling at negotiation, so video
    /// never budgets bandwidth that Opus is going to use anyway.
    ///
    /// The measured cost is ~140 Kbps: game audio is Opus at 128 Kbps
    /// (`audio.rs`) plus ~7% RTP/AES framing at 20 ms frames. The default is
    /// ~3.5x that, because the consequence of under-reserving (audio and video
    /// fighting at the moment the link saturates) is worse than the consequence
    /// of over-reserving by a few hundred Kbps. Echo's microphone travels the
    /// other direction and contends for the client's uplink, not the host's, so
    /// it is deliberately not counted here.
    ///
    /// Never claims more than a quarter of a session's ceiling — see
    /// `qos::MAX_RESERVE_FRACTION` — so raising this for a fat link cannot
    /// starve a thin one.
    pub audio_reserve_kbps: u32,

    /// Ask the router to forward Nova's WAN ports over UPnP.
    ///
    /// On by default, because the alternative is telling every user to log into
    /// their router — and a streaming host that only works on its own LAN is
    /// half a product. See `upnp.rs` for exactly which ports are opened (two)
    /// and which is deliberately never opened (48011, the LAN control port).
    ///
    /// Turn it off on a network where port forwarding is somebody else's
    /// decision — a corporate LAN, a shared house, a machine already reached
    /// through a VPN. Nothing else changes: LAN sessions are unaffected, and a
    /// relay that is reachable by other means still works.
    pub upnp: bool,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct HdrConfig {
    /// HEVC SEI type 137 (Mastering Display Colour Volume) — max panel luminance in nits.
    /// Standard HDR600 = 600, HDR1000 = 1000, HDR2000 = 2000. Default 1000.
    pub max_luminance_nits: u16,
    /// HEVC SEI type 144 (Content Light Level) MaxCLL — brightest single pixel across
    /// the entire stream, in nits. Tune to your content's measured peak. Default 1000.
    pub max_cll_nits: u16,
    /// HEVC SEI type 144 MaxFALL — maximum frame-average light level, in nits.
    /// Typically 100–400 nit for graded HDR content. Default 400.
    pub max_fall_nits: u16,
}

/// Echo side-channel (`echo_rpc.rs`) — the control/telemetry RPC for Nova's
/// native client. Absent from an existing `nova.toml` is fine: the whole table
/// is `#[serde(default)]`, so upgrading an install picks up the defaults below
/// without any config migration.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct EchoConfig {
    /// Run the Echo RPC listener at all.
    pub enabled: bool,
    /// TCP port for the newline-delimited JSON control channel (mutual TLS).
    pub port: u16,
    /// WAN signaling relay — see [`SignalingConfig`]. Absent = LAN only.
    pub signaling: SignalingConfig,
}

/// Signaling relay used for zero-config WAN connections (`echo::signaling`).
///
/// Entirely opt-in: with no `url` Nova never contacts anything, and Echo works
/// on the LAN exactly as before.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct SignalingConfig {
    /// `https://` URL of the signaling relay. Empty disables WAN signaling.
    /// Plaintext is refused — the connection carries Nova's identity
    /// certificate.
    pub url: String,
    /// SHA-256 (64 hex chars) of the relay's TLS certificate.
    ///
    /// Required whenever `url` is set: the relay is authenticated by pin, not
    /// by a public CA, so trust is one key we operate rather than every CA on
    /// the internet. Rotating the relay's certificate means shipping a new
    /// pin.
    pub relay_cert_sha256: String,
    /// How long the relay may hold a long-poll open. Clamped to 5–55 s —
    /// middleboxes commonly cut idle HTTP requests around 60 s.
    pub poll_timeout_secs: u32,

    /// The relay URL to **advertise to clients**, when it differs from the one
    /// Nova itself dials.
    ///
    /// These are genuinely two different questions and conflating them breaks
    /// one of them. `url` is how *this host* reaches the relay, and for a
    /// self-hosted relay the right answer is `https://127.0.0.1:8443/…`:
    /// loopback, always up, no NAT involved. This is how *a phone on cellular*
    /// reaches it, which has to be a public address.
    ///
    /// Setting `url` to the public address to solve the second problem creates
    /// a new one — the host would then dial its own public IP and need NAT
    /// hairpinning to talk to a relay running on the same machine.
    ///
    /// Leave empty and Nova works it out: UPnP's mapped address if the router
    /// gave one, otherwise this host's LAN address. Set it when you have
    /// forwarded a port by hand, or when your relay is behind a DNS name you
    /// maintain. An explicit value here always wins — an operator who typed an
    /// address means it.
    pub advertise_url: String,
}

// ── Defaults ──────────────────────────────────────────────────────────────────

impl Default for NovaConfig {
    fn default() -> Self {
        Self {
            stream:  StreamConfig::default(),
            audio:   AudioConfig::default(),
            network: NetworkConfig::default(),
            hdr:     HdrConfig::default(),
            echo:    EchoConfig::default(),
        }
    }
}

impl Default for EchoConfig {
    fn default() -> Self {
        Self { enabled: true, port: 48011, signaling: SignalingConfig::default() }
    }
}

impl Default for SignalingConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            relay_cert_sha256: String::new(),
            poll_timeout_secs: 30,
            advertise_url: String::new(),
        }
    }
}

impl Default for HdrConfig {
    fn default() -> Self {
        Self { max_luminance_nits: 1000, max_cll_nits: 1000, max_fall_nits: 400 }
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            width:        1920,
            height:       1080,
            bitrate_kbps: 15000,
            fps:          60,
            codec:                "h264".to_string(),
            enable_hdr:           false,
            headless_for_all_apps: true,
            detach_grace_secs:    None,
            idle_teardown_secs:   None,
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { endpoint_override: String::new() }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { fec_percentage: 10, audio_reserve_kbps: 512, upnp: true }
    }
}

// ── Loader ────────────────────────────────────────────────────────────────────

const CONFIG_FILENAME: &str = "nova.toml";

/// Template written on first run so the user has a self-documenting file.
const DEFAULT_TOML: &str = r#"# Nova Game Streaming — runtime configuration
# Edit this file to tune streaming settings without recompiling.
# Nova reloads it on every startup.

[stream]
width         = 1920    # VDD boot width  (Moonlight overrides per-session)
height        = 1080    # VDD boot height (Moonlight overrides per-session)
bitrate_kbps  = 15000   # encoder bitrate in Kbps; Moonlight may negotiate lower
fps           = 60      # boot frame rate; Moonlight negotiates the final value
codec                = "h264"  # "h264" | "hevc" | "av1"
enable_hdr           = false   # set true to allow HDR10/HEVC-Main10 even when the VDD
                                # capability query is slow to reflect HDRPlus=true
headless_for_all_apps = true   # route launching apps (Steam/Xbox/RetroArch/Virtual
                                # Desktop) through the VDD. App 1 (Desktop) ALWAYS
                                # mirrors the physical monitor and ignores this.
detach_grace_secs    = 600     # a client that vanishes without saying goodbye
                                # (network drop, app backgrounded) leaves the
                                # session DETACHED: encoding and transmission
                                # stop at once, but the virtual monitor and
                                # everything running on it are held, so a
                                # reconnect within this window resumes instantly.
                                # After it, Nova tears down as if /cancel had
                                # been sent. 0 = hold indefinitely. An explicit
                                # "End Stream" ignores this and ends immediately.
                                # (Old name idle_teardown_secs still works.)

[audio]
endpoint_override = ""  # Windows audio endpoint friendly name or GUID;
                        # leave blank to use the system default device

[network]
fec_percentage = 10     # Reed-Solomon FEC parity % (0 = disabled).
                        # Pure overhead on top of the video bitrate. 10% covers
                        # the WiFi drops that leave smeared macroblocks an
                        # infinite GOP cannot repair until the intra-refresh
                        # sweep arrives. Drop to 5 on a wired LAN; do not go
                        # near 20 at high bitrates — 20% at 4K120 added ~20
                        # Mbps of parity and saturated the link (2026-08-07).
audio_reserve_kbps = 512 # bandwidth held back from video for the Opus audio
                        # pipeline. Measured cost is ~140 Kbps (128 Kbps Opus +
                        # framing), so this is ~3.5x headroom. Never takes more
                        # than a quarter of a session's ceiling, so a thin link
                        # degrades its audio share instead of losing its picture.
upnp = true             # Ask the router to forward Nova's WAN ports, so Echo
                        # reaches this host from outside the LAN with no manual
                        # port forwarding. Opens exactly two: the relay's TCP
                        # port and the media UDP port. The Echo control port
                        # 48011 is never opened — it is LAN-only by design.
                        # Leases are finite and renewed, so a crashed Nova
                        # stops renewing and the holes close by themselves.

[echo]
# Echo side-channel — the control/telemetry RPC for Nova's native client.
# Newline-delimited JSON over mutual TLS: a client authenticates with the SAME
# certificate it paired with (nova_paired.json), so pairing is the only way in
# and "Clear Paired Devices" revokes this port too. No shared secret to manage.
enabled = true
port    = 48011

[echo.signaling]
# Zero-config WAN connections. Leave url empty for LAN-only operation — Nova
# then never contacts any external service. The relay is authenticated by
# certificate PIN (not a public CA), and Nova authenticates to it with the same
# certificate it pairs clients with.
url               = ""   # https:// URL of the signaling relay ("" = disabled)
relay_cert_sha256 = ""   # SHA-256 of the relay's TLS cert, 64 hex chars
poll_timeout_secs = 30   # long-poll hold time (clamped to 5-55)
advertise_url     = ""   # relay URL to ADVERTISE, when it differs from the one
                         # Nova dials. Leave empty and Nova works it out (UPnP
                         # address, else this host's LAN address). Set it when
                         # you forwarded a port by hand: keep `url` on loopback
                         # so the host still reaches its own relay locally.

[hdr]
# HDR10 HEVC SEI luminance parameters — tune to your TV's spec sheet.
# BT.2020 primaries are standard constants and are not configurable.
max_luminance_nits = 1000   # panel peak brightness (HDR600=600, HDR1000=1000, HDR2000=2000)
max_cll_nits       = 1000   # MaxCLL: brightest pixel in the stream (nit)
max_fall_nits      = 400    # MaxFALL: max frame-average light level (nit)
"#;

impl NovaConfig {
    /// Load `nova.toml` from the executable's directory.
    ///
    /// If the file is absent Nova writes the default template and proceeds
    /// with built-in defaults — first-run experience requires no manual setup.
    /// Parse errors are logged and built-in defaults are used so a malformed
    /// config never prevents Nova from starting.
    pub fn load() -> Self {
        let path = Self::config_path();

        if !path.exists() {
            if let Err(e) = std::fs::write(&path, DEFAULT_TOML) {
                println!("⚠️  Could not write default nova.toml ({}): {e}", path.display());
            } else {
                println!("📝 Created default config: {}", path.display());
            }
            return Self::default();
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t)  => t,
            Err(e) => {
                println!("⚠️  Could not read {} : {e} — using built-in defaults", path.display());
                return Self::default();
            }
        };

        match toml::from_str::<Self>(&text) {
            Ok(cfg) => {
                println!(
                    "⚙️  Config: {} — {}x{}@{}fps  {}  {} Kbps  fec={}%{}",
                    path.display(),
                    cfg.stream.width, cfg.stream.height, cfg.stream.fps,
                    cfg.stream.codec, cfg.stream.bitrate_kbps,
                    cfg.network.fec_percentage,
                    if cfg.stream.enable_hdr { "  HDR10=forced" } else { "" },
                );
                if !cfg.audio.endpoint_override.is_empty() {
                    println!("🔊 Audio endpoint override: \"{}\"", cfg.audio.endpoint_override);
                }
                cfg
            }
            Err(e) => {
                println!("⚠️  nova.toml parse error: {e} — using built-in defaults");
                Self::default()
            }
        }
    }

    fn config_path() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(CONFIG_FILENAME)))
            .unwrap_or_else(|| PathBuf::from(CONFIG_FILENAME))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An install that had tuned the old name must keep its tuning across the
    /// rename — silently adopting the new default would change teardown
    /// behaviour on an upgrade, which is exactly the kind of surprise a
    /// deprecated alias exists to prevent.
    #[test]
    fn the_deprecated_grace_name_is_still_honoured() {
        let legacy: NovaConfig = toml::from_str("[stream]\nidle_teardown_secs = 90\n").unwrap();
        assert_eq!(legacy.stream.detach_grace(), 90);

        // The current name wins when both are present.
        let both: NovaConfig =
            toml::from_str("[stream]\nidle_teardown_secs = 90\ndetach_grace_secs = 42\n").unwrap();
        assert_eq!(both.stream.detach_grace(), 42);

        // Absent entirely ⇒ the default, not zero. A zero here would mean
        // "hold detached sessions forever", which is the opposite of the
        // intended behaviour and would leak virtual monitors.
        let empty: NovaConfig = toml::from_str("").unwrap();
        assert_eq!(empty.stream.detach_grace(), DEFAULT_DETACH_GRACE_SECS);
        assert_eq!(NovaConfig::default().stream.detach_grace(), DEFAULT_DETACH_GRACE_SECS);

        // Explicit zero is a real choice and must survive as one.
        let never: NovaConfig = toml::from_str("[stream]\ndetach_grace_secs = 0\n").unwrap();
        assert_eq!(never.stream.detach_grace(), 0);
    }

    /// The shipped template must parse, and must agree with the built-in
    /// defaults — a template that drifts from `Default` hands new installs a
    /// different configuration from upgraded ones.
    #[test]
    fn the_default_template_parses_and_matches_the_builtin_defaults() {
        let from_template: NovaConfig =
            toml::from_str(DEFAULT_TOML).expect("shipped nova.toml template must parse");
        let built_in = NovaConfig::default();

        assert_eq!(from_template.stream.detach_grace(), built_in.stream.detach_grace());
        assert_eq!(
            from_template.network.audio_reserve_kbps,
            built_in.network.audio_reserve_kbps,
        );
        assert_eq!(from_template.network.fec_percentage, built_in.network.fec_percentage);
        assert_eq!(from_template.stream.fps, built_in.stream.fps);
    }
}
