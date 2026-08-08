//! `msgtui` — launcher for the reusable term_render chat TUI.
//!
//! The TUI frontend (`msgtui` lib) is protocol-agnostic; this binary is where
//! the messaging protocol meets the frontend: it adapts the channel-based
//! [`msgclient::Client`] to the [`msgtui::ChatClient`] trait via
//! [`tui_client::TuiClient`], then runs the app.

use std::process::ExitCode;

mod tui_client;

use tui_client::TuiClient;

const USAGE: &str =
    "usage: msgtui [--host HOST] [--port PORT] [--user USER] [--password PASSWORD] [--conv HEX]";

struct Args {
    host: String,
    port: u16,
    user: String,
    password: String,
    conv: Option<String>,
}

fn take_value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("missing value for {flag}"))
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        host: "127.0.0.1".to_string(),
        port: 9723,
        user: String::new(),
        password: String::new(),
        conv: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--host" => args.host = take_value(&mut it, &flag)?,
            "--port" => {
                args.port = take_value(&mut it, &flag)?
                    .parse()
                    .map_err(|e| format!("invalid --port: {e}"))?
            }
            "--user" => args.user = take_value(&mut it, &flag)?,
            "--password" => args.password = take_value(&mut it, &flag)?,
            "--conv" => args.conv = Some(take_value(&mut it, &flag)?),
            "--help" | "-h" => return Ok(None),
            other => return Err(format!("unknown flag: {other}\n{USAGE}")),
        }
    }
    Ok(Some(args))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::from(1);
        }
    };

    let client = match TuiClient::connect(&args.host, args.port) {
        Ok(c) => Box::new(c),
        Err(e) => {
            eprintln!("error: failed to connect to {}:{}: {e}", args.host, args.port);
            return ExitCode::from(1);
        }
    };

    if let Err(e) = msgtui::run_app(client, &args.user, &args.password, args.conv.as_deref()) {
        eprintln!("error: TUI exited with: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
