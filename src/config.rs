use std::path::PathBuf;
use serde::Deserialize;

// ── Top-level ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NovaConfig {
    pub stream:  StreamConfig,
    pub audio:   AudioConfig,
    pub network: NetworkConfig,
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
    /// Route every app (Desktop, Steam, Xbox, RetroArch, …) through the
    /// Virtual Display Driver, regardless of which app ID Moonlight launched.
    /// When false, only App 5 (Virtual Desktop) activates headless mode.
    /// Default true — universal headless is the recommended configuration.
    pub headless_for_all_apps: bool,
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
    pub fec_percentage: u32,
}

// ── Defaults ──────────────────────────────────────────────────────────────────

impl Default for NovaConfig {
    fn default() -> Self {
        Self {
            stream:  StreamConfig::default(),
            audio:   AudioConfig::default(),
            network: NetworkConfig::default(),
        }
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
        Self { fec_percentage: 20 }
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
headless_for_all_apps = true   # route ALL apps through the VDD (recommended);
                                # set false to restrict headless mode to App 5 only

[audio]
endpoint_override = ""  # Windows audio endpoint friendly name or GUID;
                        # leave blank to use the system default device

[network]
fec_percentage = 20     # Reed-Solomon FEC parity % (0 = disabled)
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
