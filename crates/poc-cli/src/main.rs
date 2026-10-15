//! Headless PoC driver.
//!
//! ```text
//! poc create    --socket <name> [--session poc]
//! poc snapshot  --socket <name> [--session poc]
//! poc stream    --socket <name> [--session poc] [--seconds 5]
//! poc reconnect --socket <name> [--session poc] [--detach-seconds 3]
//! poc leak      --socket <name> [--session poc] [--inject-bug]
//! ```
//!
//! Everything runs against a **private tmux server** (`tmux -L <name>`), so a
//! developer's own sessions and terminals are never touched.  Inside the test
//! container that server lives and dies with the container.

mod core_gate;
mod scenarios;
mod tmux;

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use tmux_client::session::{ControlSession, SessionEvent};
use tmux_client::transport::Target;
use tmux_protocol::limits::Limits;

use crate::scenarios::POC_SESSION;
use crate::tmux::Tmux;

const DEFAULT_SOCKET: &str = "poc";

struct Options {
    socket: String,
    session: String,
    seconds: u64,
    inject_bug: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            socket: DEFAULT_SOCKET.to_string(),
            session: POC_SESSION.to_string(),
            seconds: 5,
            inject_bug: false,
        }
    }
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut take_value = |name: &str| -> Result<String, String> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg {
            "--socket" => options.socket = take_value("--socket")?,
            "--session" => options.session = take_value("--session")?,
            "--seconds" => {
                options.seconds = take_value("--seconds")?
                    .parse()
                    .map_err(|_| "--seconds must be a number".to_string())?;
            }
            "--detach-seconds" => {
                options.seconds = take_value("--detach-seconds")?
                    .parse()
                    .map_err(|_| "--detach-seconds must be a number".to_string())?;
            }
            "--inject-bug" => options.inject_bug = true,
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }
    Ok(options)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().cloned() else {
        eprintln!("usage: poc <create|snapshot|stream|reconnect|leak|core-gate|doctor> [options]");
        return ExitCode::from(2);
    };
    let options = match parse_options(&args[1..]) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let tmux = Tmux::new(options.socket.clone());

    let result = match command.as_str() {
        "create" => cmd_create(&tmux, &options),
        "snapshot" => cmd_snapshot(&tmux, &options),
        "stream" => cmd_stream(&tmux, &options),
        "reconnect" => cmd_reconnect(&tmux, &options),
        "leak" => cmd_leak(&tmux, &options),
        "core-gate" => cmd_core_gate(&tmux, &options),
        "doctor" => cmd_doctor(&tmux),
        other => {
            eprintln!("unknown command: {other}");
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_doctor(tmux: &Tmux) -> Result<(), String> {
    let version = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .map_err(|err| format!("tmux not found: {err}"))?;
    println!("tmux: {}", String::from_utf8_lossy(&version.stdout).trim());
    println!("socket: {}", tmux.socket());
    println!("server running: {}", tmux.is_running());
    Ok(())
}

fn cmd_create(tmux: &Tmux, options: &Options) -> Result<(), String> {
    tmux.kill_server();
    scenarios::create_poc_session(tmux, &options.session)?;
    println!(
        "created session {} on socket {}",
        options.session,
        tmux.socket()
    );
    Ok(())
}

fn attach(tmux: &Tmux, options: &Options) -> Result<ControlSession, String> {
    let target = Target::Local {
        socket: Some(tmux.socket().to_string()),
        session: options.session.clone(),
    };
    ControlSession::attach(&target, Limits::interactive()).map_err(|err| err.to_string())
}

fn cmd_snapshot(tmux: &Tmux, options: &Options) -> Result<(), String> {
    let started = Instant::now();
    let mut session = attach(tmux, options)?;
    let events = session
        .pump_until_synced(Duration::from_secs(20))
        .map_err(|err| err.to_string())?;

    let mut screens: BTreeMap<String, usize> = BTreeMap::new();
    let mut stats = None;
    for event in events {
        match event {
            SessionEvent::PaneSnapshot { pane, lines } => {
                screens.insert(pane.to_string(), lines.len());
            }
            SessionEvent::SnapshotComplete(s) => stats = Some(s),
            SessionEvent::Desync { reason } => {
                return Err(format!("desynchronised: {reason}"));
            }
            _ => {}
        }
    }
    println!("snapshot in {:?}", started.elapsed());
    if let Some(stats) = stats {
        println!(
            "sessions={} windows={} panes={} captures={}",
            stats.sessions, stats.windows, stats.panes, stats.captures
        );
    }
    for (pane, lines) in &screens {
        println!("  {pane}: {lines} lines");
    }
    println!("consistency: {:?}", session.model().consistency());
    println!(
        "dropped output bytes: {}",
        session.model().dropped_output_bytes()
    );
    for (id, window) in session.model().windows() {
        println!(
            "  {id} index={} name={:?} panes={:?} layout={:?}",
            window.index,
            String::from_utf8_lossy(&window.name),
            window.panes,
            window
                .layout
                .as_ref()
                .map(|layout| layout.root.pane_count())
        );
    }
    session.close();
    Ok(())
}

fn cmd_stream(tmux: &Tmux, options: &Options) -> Result<(), String> {
    let mut session = attach(tmux, options)?;
    let _ = session
        .pump_until_synced(Duration::from_secs(20))
        .map_err(|err| err.to_string())?;
    println!("synced; streaming for {}s", options.seconds);
    let deadline = Instant::now() + Duration::from_secs(options.seconds);
    let mut bytes = 0usize;
    let mut chunks = 0usize;
    while Instant::now() < deadline {
        for event in session
            .pump(Duration::from_millis(200))
            .map_err(|err| err.to_string())?
        {
            if let SessionEvent::PaneOutput { pane, data } = event {
                bytes += data.len();
                chunks += 1;
                if chunks <= 20 {
                    println!(
                        "{pane}: {}",
                        String::from_utf8_lossy(&data).replace('\n', "\\n")
                    );
                }
            }
        }
    }
    println!("received {bytes} bytes in {chunks} chunks");
    println!(
        "dropped output bytes: {}",
        session.model().dropped_output_bytes()
    );
    session.close();
    Ok(())
}

fn cmd_reconnect(tmux: &Tmux, options: &Options) -> Result<(), String> {
    if !tmux.is_running() {
        scenarios::create_poc_session(tmux, &options.session)?;
    }
    println!("scenario: reconnect (session {})", options.session);
    let report = scenarios::reconnect(tmux, &options.session, options.seconds)?;
    let code = report.print();
    if code == 0 {
        Ok(())
    } else {
        Err("reconnect scenario failed".to_string())
    }
}

/// The terminal-core gate: restore every pane's snapshot and cross-check it against
/// what tmux itself reports for that pane.
fn cmd_core_gate(tmux: &Tmux, options: &Options) -> Result<(), String> {
    if !tmux.is_running() {
        scenarios::create_poc_session(tmux, &options.session)?;
    }
    println!("scenario: terminal-core gate (session {})", options.session);
    println!("preparing rich pane content ...");
    scenarios::prepare_rich_content(tmux, &options.session, "0")?;

    let (report, evidence) = core_gate::run(tmux, &options.session)?;
    println!("--- restored panes ---");
    core_gate::print_evidence(&evidence);
    println!("--- checks ---");
    let code = report.print();
    if code == 0 {
        Ok(())
    } else {
        Err("terminal-core gate failed".to_string())
    }
}

fn cmd_leak(tmux: &Tmux, options: &Options) -> Result<(), String> {
    // A dedicated session to attach to, so the scenario is self-contained.
    let leak_session = format!("{}-leak", options.session);
    tmux.run_ignoring_errors(&["kill-session", "-t", &leak_session]);
    tmux.run(&[
        "new-session",
        "-d",
        "-s",
        &leak_session,
        "-x",
        "80",
        "-y",
        "24",
        "/bin/sh",
    ])
    .map_err(|err| err.to_string())?;

    println!(
        "scenario: takeover leak (session {leak_session}, inject_bug={})",
        options.inject_bug
    );
    let (report, outcome) = scenarios::leak(tmux, &leak_session, options.inject_bug)?;
    println!("--- shell transcript ---");
    println!("{}", outcome.shell_transcript.replace('\u{1b}', "^["));
    println!("--- end transcript ---");
    let code = report.print();
    if code == 0 {
        Ok(())
    } else {
        Err("leak scenario failed".to_string())
    }
}
