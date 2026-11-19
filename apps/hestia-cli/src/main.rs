//! `hestia` — command-line front-end to the Hestia RS core. Shares its data
//! directory with the desktop app, so a host paired here shows up there.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use hestia_core::nvhttp::{PairingOutcome, PairingPin, codecs};
use hestia_core::{DeviceInfo, HestiaClient, HostRecord, HostSnapshot};
use sha2::{Digest, Sha256};

mod stream;

#[derive(Parser)]
#[command(
    name = "hestia",
    version,
    about = "Hestia RS command-line client for Hermes / Sunshine hosts"
)]
struct Cli {
    /// Data directory (identity, hosts, settings). Defaults to the app's.
    #[arg(long, global = true, env = "HESTIA_DATA_DIR")]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show this client's identity.
    Identity,
    /// Look for hosts on the local network.
    Discover {
        #[arg(long, default_value_t = 3)]
        seconds: u64,
        /// Also remember what was found.
        #[arg(long)]
        save: bool,
    },
    /// Add a host by address (IP, name, optionally :port).
    Add { address: String },
    /// List known hosts.
    List,
    /// Contact a host and print its status (and Hermes capabilities).
    Status { host: String },
    /// Pair with a host. Without --otp, type the printed PIN into the host's web UI.
    Pair {
        host: String,
        /// One-time PIN shown by Hermes/Apollo (host-initiated pairing).
        #[arg(long, requires = "passphrase")]
        otp: Option<String>,
        /// Passphrase shown together with the one-time PIN.
        #[arg(long)]
        passphrase: Option<String>,
    },
    /// List the host's apps.
    Apps { host: String },
    /// Launch (or resume) an app by id or name, without streaming it.
    Launch {
        host: String,
        app: String,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        fps: Option<u32>,
    },
    /// Launch an app and stream it headless, printing statistics every second.
    ///
    /// Video is decoded in software by default (`--dump` writes the elementary
    /// stream instead, `--count` only counts frames); audio is received and
    /// counted, or played with `--audio`. Press Enter to stop.
    Stream {
        host: String,
        app: String,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        fps: Option<u32>,
        /// Video bitrate in kbps.
        #[arg(long)]
        bitrate: Option<u32>,
        /// Stop after this many seconds (0: until Enter or the host ends it).
        #[arg(long, default_value_t = 0)]
        seconds: u64,
        /// Write the received video to FILE.h264/.h265/.obu instead of decoding.
        #[arg(long, value_name = "FILE", conflicts_with = "count")]
        dump: Option<PathBuf>,
        /// Only count frames (no decoder).
        #[arg(long)]
        count: bool,
        /// Save one decoded frame (PPM) a couple of seconds in.
        #[arg(long, value_name = "FILE.ppm", conflicts_with_all = ["dump", "count"])]
        snapshot: Option<PathBuf>,
        /// Quit the app on the host when the stream ends.
        #[arg(long)]
        quit: bool,
        /// Play the audio on this machine.
        #[arg(long)]
        audio: bool,
    },
    /// Quit the app running on the host.
    Quit { host: String },
    /// Hermes: print the diagnostics document.
    Diagnostics { host: String },
    /// Hermes: list this client's permissions.
    Permissions { host: String },
    /// Forget a host locally.
    Forget { host: String },
}

fn default_data_dir() -> PathBuf {
    directories::ProjectDirs::from("io.github", "MrOz59", "hestia-rs")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".hestia-rs"))
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn run(cli: Cli) -> Result<()> {
    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
    // A plain scan doesn't need (or create) an identity.
    if let Command::Discover { seconds, save: false } = cli.command {
        return discover(None, seconds);
    }
    let client = HestiaClient::open(&data_dir)?;

    match cli.command {
        Command::Identity => {
            let id = client.identity();
            println!("data dir:     {}", client.data_dir().display());
            println!("unique id:    {}", id.unique_id());
            println!("cert sha256:  {}", fingerprint(id.cert_der()));
        }
        Command::Discover { seconds, save } => {
            discover(save.then_some(&client), seconds)?;
        }
        Command::Add { address } => {
            let snapshot = client.add_manual(&address)?;
            print_snapshot(&snapshot);
        }
        Command::List => {
            let hosts = client.hosts();
            if hosts.is_empty() {
                println!("no hosts yet; try `hestia discover --save` or `hestia add <address>`");
            }
            for h in hosts {
                println!(
                    "{:<24} {:<22} {:<8} {}",
                    h.name,
                    format!("{}:{}", h.address, h.http_port),
                    if h.is_paired() { "paired" } else { "unpaired" },
                    h.id
                );
            }
        }
        Command::Status { host } => {
            let id = resolve(&client, &host)?.id;
            print_snapshot(&client.refresh(&id)?);
        }
        Command::Pair { host, otp, passphrase } => {
            let id = resolve(&client, &host)?.id;
            let (pin, passphrase) = match otp {
                Some(otp) => (
                    PairingPin::parse(&otp).ok_or("the one-time PIN must be at least 4 digits")?,
                    passphrase,
                ),
                None => {
                    let pin = PairingPin::random();
                    println!("Enter this PIN in the host's web UI (PIN tab): {pin}");
                    println!("Waiting for the host...");
                    (pin, None)
                }
            };
            match client.pair(&id, &pin, passphrase.as_deref())?.0 {
                PairingOutcome::Paired { server_cert_der } => {
                    println!("Paired. Host certificate sha256: {}", fingerprint(&server_cert_der));
                }
                PairingOutcome::WrongPin => return Err("wrong PIN".into()),
                PairingOutcome::AlreadyInProgress => {
                    return Err("another pairing is in progress on the host; try again".into());
                }
                PairingOutcome::Failed(why) => return Err(why.into()),
            }
        }
        Command::Apps { host } => {
            let id = resolve(&client, &host)?.id;
            for app in client.apps(&id)? {
                println!(
                    "{:>12}  {}{}",
                    app.id,
                    app.title,
                    if app.hdr_supported { "  [HDR]" } else { "" }
                );
            }
        }
        Command::Launch {
            host,
            app,
            width,
            height,
            fps,
        } => {
            let id = resolve(&client, &host)?.id;
            let apps = client.apps(&id)?;
            let app = apps
                .iter()
                .find(|a| a.id.to_string() == app || a.title.eq_ignore_ascii_case(&app))
                .ok_or_else(|| format!("no app matching '{app}'"))?;
            let mut settings = client.settings();
            settings.width = width.unwrap_or(settings.width);
            settings.height = height.unwrap_or(settings.height);
            settings.fps = fps.unwrap_or(settings.fps);
            let session = client.launch_with(&id, app.id, &DeviceInfo::default(), settings)?;
            println!(
                "{} '{}' on {}",
                if session.response.resumed {
                    "Resumed"
                } else {
                    "Launched"
                },
                app.title,
                session.host.name
            );
            println!("RTSP session: {}", session.response.session_url);
            if let Some(prepared) = &session.prepared {
                let vd = &prepared.virtual_display;
                println!(
                    "Hermes session {}: display '{}' ({}) {}x{}@{}{}",
                    prepared.session_id,
                    vd.name,
                    vd.backend,
                    vd.width,
                    vd.height,
                    vd.fps,
                    if vd.hdr { " HDR" } else { "" }
                );
            }
            println!("Note: the streaming pipeline is still in development; use `hestia quit` to stop the app.");
        }
        Command::Stream {
            host,
            app,
            width,
            height,
            fps,
            bitrate,
            seconds,
            dump,
            count,
            snapshot,
            quit,
            audio,
        } => {
            let id = resolve(&client, &host)?.id;
            let mut settings = client.settings();
            settings.width = width.unwrap_or(settings.width);
            settings.height = height.unwrap_or(settings.height);
            settings.fps = fps.unwrap_or(settings.fps);
            settings.bitrate_kbps = bitrate.unwrap_or(settings.bitrate_kbps);
            let mode = if dump.is_some() {
                stream::VideoMode::Dump
            } else if count {
                stream::VideoMode::Count
            } else {
                stream::VideoMode::Decode
            };
            stream::run(
                &client,
                &id,
                stream::Options {
                    app,
                    settings,
                    seconds,
                    mode,
                    dump,
                    snapshot,
                    quit_after: quit,
                    play_audio: audio,
                },
            )?;
        }
        Command::Quit { host } => {
            let id = resolve(&client, &host)?.id;
            client.quit(&id)?;
            println!("Quit requested.");
        }
        Command::Diagnostics { host } => {
            let id = resolve(&client, &host)?.id;
            let value = client.with_api(&id, |api| api.diagnostics())??;
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Command::Permissions { host } => {
            let id = resolve(&client, &host)?.id;
            let perms = client.with_api(&id, |api| api.client_permissions())??;
            println!("{}", serde_json::to_string_pretty(&perms)?);
        }
        Command::Forget { host } => {
            let record = resolve(&client, &host)?;
            client.forget(&record.id)?;
            println!("Forgot {}.", record.name);
        }
    }
    Ok(())
}

fn discover(client: Option<&HestiaClient>, seconds: u64) -> Result<()> {
    println!("Searching for {seconds}s...");
    let found = hestia_core::discovery::scan(Duration::from_secs(seconds))?;
    if found.is_empty() {
        println!("No hosts answered. Add one by address with `hestia add <ip>`.");
    }
    for host in &found {
        let addrs: Vec<String> = host.addresses.iter().map(|a| a.to_string()).collect();
        println!(
            "{:<24} {}:{}  ({})",
            host.name,
            addrs.join(", "),
            host.port,
            host.hostname
        );
        if let Some(client) = client {
            let id = client.note_discovered(host)?;
            if let Err(e) = client.refresh(&id) {
                eprintln!("  could not contact {}: {e}", host.name);
            }
        }
    }
    Ok(())
}

fn resolve(client: &HestiaClient, query: &str) -> Result<HostRecord> {
    let hosts = client.hosts();
    let matches: Vec<&HostRecord> = hosts
        .iter()
        .filter(|h| h.id == query || h.name.eq_ignore_ascii_case(query) || h.address == query)
        .collect();
    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => {
            // Allow addressing a host that isn't saved yet.
            let snapshot = client.add_manual(query)?;
            Ok(snapshot.record)
        }
        _ => Err(format!("'{query}' matches several hosts; use the id from `hestia list`").into()),
    }
}

fn print_snapshot(s: &HostSnapshot) {
    let r = &s.record;
    println!("{} ({}:{})", r.name, r.address, r.http_port);
    println!("  id:       {}", r.id);
    println!("  online:   {}", if s.online { "yes" } else { "no" });
    if let Some(e) = &s.error {
        println!("  error:    {e}");
    }
    println!(
        "  paired:   {}",
        match (r.is_paired(), s.paired) {
            (true, true) => "yes",
            (true, false) => "no (the host no longer accepts this client; pair again)",
            _ => "no",
        }
    );
    if let Some(info) = &s.info {
        println!("  version:  {}", info.app_version);
        println!("  codecs:   {}", codecs::describe(info.codec_mode_support).join(", "));
        if info.virtual_display_capable {
            println!(
                "  virtual display: capable{}",
                if info.virtual_display_driver_ready {
                    ", driver ready"
                } else {
                    ""
                }
            );
        }
        if info.is_busy() {
            println!("  running app id: {}", info.current_game);
        }
    }
    match &s.capabilities {
        Some(caps) => {
            let f = &caps.features;
            println!(
                "  Hermes {} (Hestia protocol {})",
                caps.server_version, caps.hestia_protocol
            );
            let mut feats = Vec::new();
            if f.virtual_display {
                feats.push(format!("virtual display [{}]", f.virtual_display_backend.join("/")));
            }
            for (on, name) in [
                (f.hdr_mode_control, "HDR control"),
                (f.gamescope_session, "Gamescope sessions"),
                (f.clipboard_sync, "clipboard sync"),
                (f.server_commands, "server commands"),
                (f.display_recovery, "display recovery"),
                (f.scale_factor, "scale factor"),
            ] {
                if on {
                    feats.push(name.to_owned());
                }
            }
            println!("  features: {}", feats.join(", "));
            println!(
                "  limits:   {}x{} @ {} fps, codecs {}",
                caps.limits.max_width,
                caps.limits.max_height,
                caps.limits.max_fps,
                caps.limits.supported_codecs.join("/")
            );
        }
        None if s.paired => println!("  Hermes extensions: not available (plain GameStream host)"),
        None => {}
    }
}

fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}
