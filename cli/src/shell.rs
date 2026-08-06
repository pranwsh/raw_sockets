//! `msgcli shell` — launches the reusable term_render TUI frontend wired to
//! the messaging server through the [`crate::tui_client::TuiClient`] adapter.
//!
//! The TUI itself (`msgtui` crate) is protocol-agnostic; this module is where
//! the messaging protocol meets the frontend.

use std::process::ExitCode;

use crate::tui_client::TuiClient;

pub fn run(host: &str, port: u16, user: &str, password: &str, conv_hex: Option<&str>) -> ExitCode {
    let client = match TuiClient::connect(host, port) {
        Ok(c) => Box::new(c),
        Err(e) => {
            eprintln!("error: failed to connect to {host}:{port}: {e}");
            return ExitCode::from(1);
        }
    };

    if let Err(e) = msgtui::run_app(client, user, password, conv_hex) {
        eprintln!("error: TUI exited with: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
