mod audio;
mod server;
mod tls;
mod update_check;

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Ring-buffer depth, sized at a nominal 48 kHz (the rate browsers capture at).
/// The ring is allocated once at startup and never resized, so its capacity is
/// fixed at this nominal rate. ~500ms comfortably absorbs Wi-Fi jitter; actual
/// latency is governed by PREBUFFER_MS + the latency-recovery threshold, not this
/// ceiling.
const RING_BUFFER_MS: usize = 500;
const RING_BUFFER_SAMPLES: usize = 48_000 * RING_BUFFER_MS / 1000;

/// After Ctrl+C the HTTP API keeps replying 503 for this long so a streaming
/// client's ~1s liveness poll reliably observes the shutdown before the process
/// exits. The transport close event is unreliable/late on iOS Safari, so this
/// HTTP signal — not a close frame — is what clients actually detect.
const SHUTDOWN_GRACE_PERIOD: std::time::Duration = std::time::Duration::from_millis(1200);

/// Noise-gate threshold range accepted on the CLI, in dB. Mirrors the web UI
/// slider: -100 dB disables the gate, 0 dB is the maximum.
const NOISE_GATE_DB_MIN: f32 = -100.0;
const NOISE_GATE_DB_MAX: f32 = 0.0;

/// Convert a noise-gate threshold from dB (CLI / web-UI units) to the linear
/// amplitude used internally by the audio thread and HTTP API. Mirrors the
/// client's `dbToNoiseGate`: the value is clamped to [-100, 0] dB and -100 dB maps
/// to a hard 0.0 (gate disabled), rather than to a tiny-but-nonzero amplitude.
fn noise_gate_db_to_linear(db: f32) -> f32 {
    let db = db.clamp(NOISE_GATE_DB_MIN, NOISE_GATE_DB_MAX);
    if db <= NOISE_GATE_DB_MIN {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

/// Address ranges that can never serve as a LAN pairing target reachable from
/// another device. The load-bearing exclusion is 198.18.0.0/15 — the IETF
/// benchmarking range (RFC 2544) that fake-ip proxy TUNs (mihomo/Clash,
/// sing-box, Surge, etc.) all use as their default fake-ip range: the TUN
/// adapter holding e.g. 198.18.0.1 is a fake address that nothing on the real
/// LAN can route to.
fn is_unusable_lan_addr(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 0 // 0.0.0.0/8 "this network"
                || o[0] == 127 // loopback
                || (o[0] == 169 && o[1] == 254) // link-local (APIPA)
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // RFC 2544 benchmark = fake-ip
                || o[0] >= 224 // multicast / reserved
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // Link-local IPv6 needs a zone id to route and is explicitly out
                // of scope for QuicMic's pairing URL.
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Rank how usable an address is as a LAN pairing target; higher wins. RFC 1918
/// private addresses are the most likely to be reachable by a phone on the same
/// LAN (2). CGNAT (100.64/10, used by Tailscale and some ISPs) and any IPv6
/// (global/ULA) are the second tier (1). Public IPv4 is the last resort (0).
/// The caller breaks rank ties in favour of IPv4, so a same-rank IPv4 always
/// beats a same-rank IPv6 regardless of enumeration order.
fn lan_addr_rank(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            let rfc1918 = o[0] == 10
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168);
            if rfc1918 {
                2
            } else if o[0] == 100 && (64..=127).contains(&o[1]) {
                1
            } else {
                0
            }
        }
        IpAddr::V6(_) => 1,
    }
}

/// Pick the best LAN address from an interface scan: drop unusable ranges, then
/// prefer the highest-ranked address class, breaking rank ties in favour of
/// IPv4 (matching the historical IPv4-first detection).
///
/// Two same-rank, same-family candidates (e.g. a real Wi-Fi `192.168.x` and a
/// Hyper-V/VPN `172.x`) are both valid and are resolved by `max_by_key`'s
/// "last maximum" rule — i.e. OS enumeration order — which is reproducible per
/// boot but not semantically meaningful. This only happens on the fallback path
/// (when the default-route pick was unusable); `--ip` remains the manual
/// override for multi-homed setups where the wrong address would be advertised.
fn pick_lan_ip(ifas: Vec<(String, IpAddr)>) -> Option<IpAddr> {
    ifas.into_iter()
        .map(|(_name, ip)| ip)
        .filter(|ip| !is_unusable_lan_addr(*ip))
        .max_by_key(|ip| (lan_addr_rank(*ip), ip.is_ipv4()))
}

/// Detect an IP address that other devices on the LAN can actually reach.
///
/// `local_ip_address::local_ip()` returns the first IPv4 unicast address among
/// the adapters that own a default route. That is usually right, but goes wrong
/// when a proxy TUN in fake-ip mode (mihomo/Clash, sing-box, Surge, etc.) owns
/// a default route with a low metric: the TUN's fake address (198.18.0.1) wins
/// even though nothing on the LAN can route to it. So the default-route pick is
/// trusted only
/// when it is a usable LAN address; otherwise every interface is scanned and the
/// best-ranked candidate (typically the real Ethernet/Wi-Fi address) is used.
fn detect_lan_ip() -> anyhow::Result<IpAddr> {
    if let Ok(ip) = local_ip_address::local_ip() {
        if !is_unusable_lan_addr(ip) {
            return Ok(ip);
        }
    }
    let ifas = local_ip_address::list_afinet_netifas()
        .map_err(|e| anyhow::anyhow!("Failed to enumerate network interfaces: {e}"))?;
    pick_lan_ip(ifas)
        .ok_or_else(|| anyhow::anyhow!("No usable LAN address found among the network interfaces"))
}

/// QuicMic — Turn any device with a microphone and a web browser into a wireless PC microphone.
#[derive(Parser)]
#[command(name = "quicmic", version, about)]
struct Cli {
    /// Port for both HTTPS (TCP) and WebTransport (UDP).
    #[arg(short, long, default_value = "8443", env = "QUICMIC_PORT")]
    port: u16,

    /// Audio output device name (substring match, case-insensitive).
    /// Defaults to "CABLE Input" (Windows), "BlackHole" (macOS), or "VirtualQuicMic" (Linux).
    #[arg(short, long, env = "QUICMIC_DEVICE")]
    device: Option<String>,

    /// Override the auto-detected LAN IP address. Accepts a bare or bracketed IPv6
    /// literal (e.g. `fe80::1` or `[fe80::1]`).
    #[arg(long)]
    ip: Option<String>,

    /// Set a custom 6-digit pairing PIN (auto-generated if omitted).
    #[arg(long)]
    pin: Option<String>,

    /// Dump TLS certificates to the certs/ directory for debugging.
    #[arg(long)]
    dump_certs: bool,

    /// Initial noise gate threshold in dB: -100 = Off, 0 = max (default -50).
    /// Mirrors the web UI slider; can be adjusted at runtime from the phone.
    #[arg(long, default_value = "-50", allow_hyphen_values = true)]
    noise_gate: f32,

    /// Initial audio gain multiplier (1.0 = unity, e.g. 1.5).
    /// Can be adjusted at runtime via the web UI.
    #[arg(long, default_value = "1.0")]
    gain: f32,

    /// Initial latency-recovery threshold in milliseconds (0 = disabled).
    /// When the output buffer grows past this, the oldest audio is skipped to
    /// catch up. Can be adjusted at runtime via the web UI.
    #[arg(long, default_value = "150")]
    latency_threshold: u32,

    /// List available audio output devices and exit.
    #[arg(long)]
    list_devices: bool,

    /// Disable the startup check for a newer release on GitHub.
    #[arg(long, env = "QUICMIC_NO_UPDATE_CHECK")]
    no_update_check: bool,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        use std::io::IsTerminal;
        // Print the full error chain so the cause is visible even when the app
        // was double-clicked (where the console closes the instant we exit).
        eprintln!("\nError: {e:?}");
        // Pause only when double-clicked AND attached to a real terminal, so we
        // never block in a piped / CI / non-interactive context.
        if launched_by_double_click() && std::io::stdin().is_terminal() {
            pause_before_exit();
        }
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    // Install the default CryptoProvider for rustls (using ring)
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("Failed to install rustls CryptoProvider"))?;

    // Initialize structured logging
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();

    if cli.list_devices {
        println!("Available audio output devices:");
        for (i, name) in audio::list_output_devices().iter().enumerate() {
            println!("  [{}] {}", i, name);
        }
        return Ok(());
    }

    set_terminal_title("QuicMic");

    // ── Detect LAN IP ───────────────────────────────────────────────────
    let lan_ip: IpAddr = match &cli.ip {
        Some(ip) => parse_ip_arg(ip)?,
        None => detect_lan_ip()?,
    };

    // ── List audio devices ──────────────────────────────────────────────
    let devices = audio::list_output_devices();
    info!("Available audio output devices:");
    for (i, name) in devices.iter().enumerate() {
        info!("  [{}] {}", i, name);
    }

    // ── Create shared ring buffer ───────────────────────────────────────
    let ring = Arc::new(audio::RingBuffer::new(RING_BUFFER_SAMPLES));

    // ── Shared atomics ──────────────────────────────────────────────────
    let is_connected = Arc::new(AtomicBool::new(false));
    let session_token: Arc<parking_lot::Mutex<Option<String>>> =
        Arc::new(parking_lot::Mutex::new(None));
    // --noise-gate is given in dB (-100 = Off) to match the web UI slider; convert
    // it to the linear amplitude the audio thread and HTTP API use. The linear
    // clamp is kept as a final safety net.
    let noise_gate = Arc::new(AtomicU32::new(
        noise_gate_db_to_linear(cli.noise_gate)
            .clamp(server::NOISE_GATE_MIN, server::NOISE_GATE_MAX)
            .to_bits(),
    ));
    let gain = Arc::new(AtomicU32::new(
        cli.gain.clamp(server::GAIN_MIN, server::GAIN_MAX).to_bits(),
    ));
    let latency_threshold = Arc::new(AtomicU32::new(
        cli.latency_threshold.min(server::LATENCY_THRESHOLD_MAX_MS),
    ));
    let packets_received = Arc::new(AtomicU64::new(0));
    let packets_lost = Arc::new(AtomicU64::new(0));
    let source_sample_rate = Arc::new(AtomicU32::new(48000));
    let is_shutdown = Arc::new(AtomicBool::new(false));
    let device_ok = Arc::new(AtomicBool::new(false));
    // Holds the latest newer release tag once the startup update check finds one
    // (stays None otherwise). Surfaced via `/api/info` for the web UI banner.
    let update_status: Arc<parking_lot::Mutex<Option<String>>> =
        Arc::new(parking_lot::Mutex::new(None));

    // ── Start audio output (supervised: auto-rebuilds if the device drops) ──
    audio::spawn_output_supervisor(
        cli.device.clone(),
        ring.clone(),
        source_sample_rate.clone(),
        latency_threshold.clone(),
        device_ok.clone(),
    )?;

    // ── Generate TLS identity ───────────────────────────────────────────
    let (wt_identity, identity) = tls::generate_identity(lan_ip, cli.dump_certs)?;

    // ── Generate pairing PIN ────────────────────────────────────────────
    let pin = match cli.pin {
        Some(pin) => {
            if pin.len() != 6 || !pin.bytes().all(|b| b.is_ascii_digit()) {
                anyhow::bail!("--pin must be exactly 6 digits (0-9)");
            }
            pin
        }
        None => format!("{:06}", rand::random_range(0..1_000_000u32)),
    };

    // ── Print startup banner ────────────────────────────────────────────
    let url = format!("https://{}:{}", url_host(&lan_ip), cli.port);
    print_banner(&url, &pin, &identity.cert_hash_base64);

    // Print QR code for easy mobile pairing (URL includes PIN as hash fragment)
    let qr_url = format!("{}#{}", url, pin);
    if let Err(e) = qr2term::print_qr(&qr_url) {
        info!("Could not print QR code: {}", e);
    }

    // Background, opt-out check for a newer release. Never blocks startup and stays
    // silent unless a strictly newer version is found.
    if !cli.no_update_check {
        let slot = update_status.clone();
        tokio::spawn(async move {
            if let Some(tag) = update_check::latest_if_newer().await {
                info!(
                    "A newer version {} is available (current {}). Releases: {}",
                    tag,
                    env!("CARGO_PKG_VERSION"),
                    update_check::releases_url()
                );
                *slot.lock() = Some(tag);
            }
        });
    }

    let (cancel_tx, _) = tokio::sync::broadcast::channel(16);

    // ── Build shared stream state ───────────────────────────────────────
    let stream_state = server::StreamState {
        ring: ring.clone(),
        is_connected: is_connected.clone(),
        session_token: session_token.clone(),
        noise_gate: noise_gate.clone(),
        gain: gain.clone(),
        latency_threshold: latency_threshold.clone(),
        packets_received: packets_received.clone(),
        packets_lost: packets_lost.clone(),
        source_sample_rate: source_sample_rate.clone(),
        cancel_tx,
        is_shutdown: is_shutdown.clone(),
        device_ok: device_ok.clone(),
    };

    // ── Build axum app ──────────────────────────────────────────────────
    let app_state = server::AppState {
        stream: stream_state.clone(),
        tls_identity: identity.clone(),
        pairing_pin: pin,
        wt_port: cli.port,
        lan_ip: lan_ip.to_string(),
        pairing_throttle: Arc::new(parking_lot::Mutex::new(server::PairingThrottle::default())),
        update_status,
    };

    let router = server::build_router(app_state);
    let tls_config = tls::build_rustls_config_async(&identity).await?;
    let https_addr = SocketAddr::new(lan_ip, cli.port);

    let axum_handle = axum_server::Handle::new();
    let axum_handle_clone = axum_handle.clone();

    // Launch HTTPS server
    let https_task = tokio::spawn(server::run_https_server(
        https_addr,
        router,
        tls_config,
        axum_handle_clone,
    ));

    // Launch WebTransport server
    let wt_task = tokio::spawn(server::run_webtransport_server(
        wt_identity,
        cli.port,
        stream_state.clone(),
    ));

    // Listen for Ctrl+C to shutdown gracefully
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("Graceful shutdown initiated.");

            let had_client = stream_state.is_connected.load(Ordering::SeqCst);

            // Flip the HTTP API to 503 and end the active session. Clients detect
            // the shutdown by polling the API — their transport close event is
            // unreliable/late on iOS Safari — so the only thing that matters is
            // keeping the API up (replying 503) long enough for that poll to land.
            stream_state.is_shutdown.store(true, Ordering::SeqCst);
            let _ = stream_state.cancel_tx.send(());

            if had_client {
                tokio::time::sleep(SHUTDOWN_GRACE_PERIOD).await;
            }

            axum_handle.graceful_shutdown(Some(std::time::Duration::from_millis(200)));
            info!("Graceful shutdown complete. Exiting.");
        }
        res = https_task => {
            // The HTTPS server returns only on a bind failure or crash — fatal,
            // so surface it instead of exiting silently.
            res.map_err(|e| anyhow::anyhow!("HTTPS server task failed: {e}"))??;
        }
        res = wt_task => {
            res.map_err(|e| anyhow::anyhow!("WebTransport server task failed: {e}"))??;
        }
    }

    Ok(())
}

/// Parse the `--ip` argument into an `IpAddr`, tolerating a bracketed IPv6 literal
/// (`[fe80::1]`) since that is how it appears in a URL and how a user is likely to
/// copy it. Brackets are stripped only as a matched pair; anything else is parsed
/// as-is, and a clear error names the offending value.
fn parse_ip_arg(s: &str) -> anyhow::Result<IpAddr> {
    let bare = s
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(s);
    bare.parse::<IpAddr>()
        .map_err(|e| anyhow::anyhow!("invalid --ip value '{s}': {e}"))
}

/// Format an IP address for use in a URL authority, bracketing IPv6 literals
/// (`https://[fe80::1]:8443`) as required by RFC 3986. IPv4 is returned unchanged.
/// The client applies the same bracketing when it builds the WebTransport URL.
fn url_host(ip: &IpAddr) -> String {
    match ip {
        IpAddr::V6(_) => format!("[{ip}]"),
        IpAddr::V4(_) => ip.to_string(),
    }
}

fn print_banner(url: &str, pin: &str, cert_hash: &str) {
    let version = format!("QuicMic v{}", env!("CARGO_PKG_VERSION"));
    let cert_prefix = if cert_hash.len() >= 20 {
        &cert_hash[..20]
    } else {
        cert_hash
    };

    // Every row is padded to a fixed inner width so the borders always line up,
    // regardless of the URL / PIN / hash lengths.
    const W: usize = 59;
    let row = |s: &str| println!("║ {:<width$} ║", s, width = W - 2);
    let step = |s: &str| println!("║ │ {:<width$} │ ║", s, width = W - 6);

    println!();
    println!("╔{}╗", "═".repeat(W));
    row(&version);
    row("");
    row(&format!("URL:          {}", url));
    row(&format!("Pairing PIN:  {}", pin));
    row(&format!("Cert SHA-256: {}", cert_prefix));
    row("");
    println!("║ ┌{}┐ ║", "─".repeat(W - 4));
    step("Setup instructions:");
    step("1. Scan the QR code below with your phone camera");
    step(&format!("2. Or open: {}", url));
    step(&format!("   and enter PIN: {}", pin));
    println!("║ └{}┘ ║", "─".repeat(W - 4));
    println!("╚{}╝", "═".repeat(W));
    println!();
}

/// Set the terminal window title, best-effort and cross-platform.
fn set_terminal_title(title: &str) {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;

        // `SetConsoleTitleW` (kernel32, already linked by std) sets the title in
        // cmd, PowerShell, conhost, and Windows Terminal without needing virtual
        // terminal processing to be enabled.
        #[link(name = "kernel32")]
        extern "system" {
            fn SetConsoleTitleW(title: *const u16) -> i32;
        }

        let wide: Vec<u16> = OsStr::new(title)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is a valid NUL-terminated UTF-16 buffer that outlives the call.
        unsafe {
            SetConsoleTitleW(wide.as_ptr());
        }
    }
    #[cfg(not(windows))]
    {
        use std::io::Write;
        // OSC 0 sets the window title in xterm, GNOME Terminal, Terminal.app,
        // iTerm2, and most other terminals.
        print!("\x1b]0;{title}\x07");
        let _ = std::io::stdout().flush();
    }
}

/// Best-effort detection of whether the app was launched by double-clicking
/// (rather than from an existing terminal). Used to keep the window open on a
/// startup error so the user can read it.
#[cfg(windows)]
fn launched_by_double_click() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleProcessList(lpdwProcessList: *mut u32, dwProcessCount: u32) -> u32;
    }
    let mut buffer = [0u32; 2];
    // SAFETY: GetConsoleProcessList writes up to `buffer.len()` entries into the
    // valid stack buffer and returns the number of processes on the console.
    let count = unsafe { GetConsoleProcessList(buffer.as_mut_ptr(), buffer.len() as u32) };
    // Only our own process is attached -> the console was created just for us.
    count <= 1
}

#[cfg(not(windows))]
fn launched_by_double_click() -> bool {
    false
}

/// Wait for the user to press Enter, so a console window created just for this
/// process doesn't vanish before an error message can be read.
fn pause_before_exit() {
    use std::io::Write;
    print!("\nPress Enter to exit...");
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
}

#[cfg(test)]
mod tests {
    use super::{
        is_unusable_lan_addr, lan_addr_rank, noise_gate_db_to_linear, parse_ip_arg, pick_lan_ip,
        url_host,
    };
    use std::net::IpAddr;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::new(a, b, c, d))
    }

    fn v6(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn fake_ip_benchmark_range_is_unusable() {
        // Fake-ip proxy TUNs (mihomo/Clash, sing-box, Surge, etc.) use the RFC
        // 2544 benchmark range 198.18/15 as their default fake-ip range.
        assert!(is_unusable_lan_addr(v4(198, 18, 0, 1)));
        assert!(is_unusable_lan_addr(v4(198, 19, 255, 255)));
        // Real LAN addresses stay usable.
        assert!(!is_unusable_lan_addr(v4(192, 168, 1, 16)));
        assert!(!is_unusable_lan_addr(v4(10, 0, 0, 5)));
    }

    #[test]
    fn loopback_link_local_and_multicast_are_unusable() {
        assert!(is_unusable_lan_addr(v4(127, 0, 0, 1)));
        assert!(is_unusable_lan_addr(v4(169, 254, 10, 20)));
        assert!(is_unusable_lan_addr(v4(224, 0, 0, 1)));
        assert!(is_unusable_lan_addr(v6("::1")));
        assert!(is_unusable_lan_addr(v6("fe80::1")));
    }

    #[test]
    fn ranking_prefers_private_then_cgnat_then_public() {
        assert!(lan_addr_rank(v4(192, 168, 1, 16)) > lan_addr_rank(v4(100, 64, 0, 2)));
        assert!(lan_addr_rank(v4(10, 1, 1, 1)) > lan_addr_rank(v4(100, 64, 0, 2)));
        assert!(lan_addr_rank(v4(100, 64, 0, 2)) > lan_addr_rank(v4(8, 8, 8, 8)));
    }

    #[test]
    fn picker_skips_tun_fake_ip_and_prefers_the_real_lan() {
        // The bug pattern: a proxy TUN adapter ("sekai") owns the fake-ip
        // address 198.18.0.1; the real Ethernet is 192.168.1.16. The picker must
        // return the real LAN address, never the fake one.
        let ifas = vec![
            ("sekai".to_string(), v4(198, 18, 0, 1)),
            ("Ethernet".to_string(), v4(192, 168, 1, 16)),
        ];
        assert_eq!(pick_lan_ip(ifas), Some(v4(192, 168, 1, 16)));
    }

    #[test]
    fn picker_returns_none_when_every_address_is_unusable() {
        let ifas = vec![
            ("tun".to_string(), v4(198, 18, 0, 1)),
            ("lo".to_string(), v4(127, 0, 0, 1)),
        ];
        assert_eq!(pick_lan_ip(ifas), None);
    }

    #[test]
    fn picker_falls_back_to_ipv6_when_only_v6_is_usable() {
        let ifas = vec![
            ("tun".to_string(), v4(198, 18, 0, 1)),
            ("eth".to_string(), v6("fd00::1")),
        ];
        assert_eq!(pick_lan_ip(ifas), Some(v6("fd00::1")));
    }

    #[test]
    fn parse_ip_arg_accepts_bare_and_bracketed() {
        assert_eq!(
            parse_ip_arg("192.168.1.42").unwrap().to_string(),
            "192.168.1.42"
        );
        assert_eq!(parse_ip_arg("fe80::1").unwrap().to_string(), "fe80::1");
        // A bracketed IPv6 literal (as copied from a URL) is accepted.
        assert_eq!(parse_ip_arg("[fe80::1]").unwrap().to_string(), "fe80::1");
        assert_eq!(parse_ip_arg("[::1]").unwrap().to_string(), "::1");
        // Unbalanced brackets / non-addresses are rejected with a clear error.
        assert!(parse_ip_arg("[fe80::1").is_err());
        assert!(parse_ip_arg("not-an-ip").is_err());
    }

    #[test]
    fn url_host_brackets_ipv6_only() {
        // IPv4 is unchanged; IPv6 literals are bracketed for the URL authority.
        assert_eq!(
            url_host(&"192.168.1.42".parse::<IpAddr>().unwrap()),
            "192.168.1.42"
        );
        assert_eq!(url_host(&"::1".parse::<IpAddr>().unwrap()), "[::1]");
        assert_eq!(url_host(&"fe80::1".parse::<IpAddr>().unwrap()), "[fe80::1]");
    }

    #[test]
    fn noise_gate_db_floor_disables_gate() {
        // -100 dB (and anything below it, after clamping) maps to a hard 0.0, i.e.
        // the gate is disabled rather than a tiny-but-nonzero amplitude.
        assert_eq!(noise_gate_db_to_linear(-100.0), 0.0);
        assert_eq!(noise_gate_db_to_linear(-250.0), 0.0);
    }

    #[test]
    fn noise_gate_db_max_is_unity() {
        // 0 dB is full scale; values above are clamped down to it.
        assert!((noise_gate_db_to_linear(0.0) - 1.0).abs() < 1e-6);
        assert!((noise_gate_db_to_linear(12.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn noise_gate_db_converts_linearly() {
        // -6 dB ≈ 0.501 in linear amplitude (10^(-6/20)).
        assert!((noise_gate_db_to_linear(-6.0) - 0.5011872).abs() < 1e-4);
        // -20 dB is exactly 0.1.
        assert!((noise_gate_db_to_linear(-20.0) - 0.1).abs() < 1e-5);
    }
}
