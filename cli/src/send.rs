//! `msgcli send` — one-shot: connect, authenticate, (create a conversation or
//! open an existing one), send a single message, then exit.
//!
//! Built on the channel-based [`msgclient::Client`]; this crate no longer speaks
//! the wire protocol directly.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use msgclient::{Action, Client, Event};

/// how long to wait for any single server reply before giving up
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

pub fn run(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    conv: Option<&str>,
    create: Option<&str>,
    message: &str,
    listen: bool,
) -> ExitCode {
    let client = match Client::connect(host, port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to connect to {host}:{port}: {e}");
            return ExitCode::from(1);
        }
    };

    // 1. authenticate
    if !client.send(Action::Hello { user: user.to_string(), password: password.to_string() }) {
        eprintln!("error: connection closed before auth");
        return ExitCode::from(1);
    }
    match wait_event(&client) {
        Some(Event::AuthOk { created }) => {
            if created {
                eprintln!("auth ok (new account created)");
            } else {
                eprintln!("auth ok (authenticated)");
            }
        }
        Some(Event::AuthFail { reason }) => {
            eprintln!("error: auth failed: {reason}");
            return ExitCode::from(1);
        }
        Some(ev) => {
            eprintln!("error: unexpected response to hello: {ev:?}");
            return ExitCode::from(1);
        }
        None => {
            eprintln!("error: timed out waiting for auth response");
            return ExitCode::from(1);
        }
    }

    // 2. resolve the target conversation id
    let conv_id = if let Some(members) = create {
        let list: Vec<String> = members
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if list.is_empty() {
            eprintln!("error: --create needs at least one member");
            return ExitCode::from(1);
        }
        if !client.send(Action::CreateConv { members: list.clone() }) {
            eprintln!("error: connection closed before create");
            return ExitCode::from(1);
        }
        match wait_event(&client) {
            Some(Event::ConvCreated { id }) => {
                eprintln!("created conversation: {} (members: {},{})", hex::encode(&id), user, members);
                id
            }
            Some(Event::Error { msg }) => {
                eprintln!("error: create conversation failed: {msg}");
                return ExitCode::from(1);
            }
            Some(ev) => {
                eprintln!("error: unexpected response to create: {ev:?}");
                return ExitCode::from(1);
            }
            None => {
                eprintln!("error: timed out waiting for ConvCreated");
                return ExitCode::from(1);
            }
        }
    } else if let Some(hex_str) = conv {
        match hex::decode(hex_str) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("error: invalid conv hex: {e}");
                return ExitCode::from(1);
            }
        }
    } else {
        unreachable!()
    };

    // 3. send the message
    if !client.send(Action::Send { conv: conv_id.clone(), text: message.to_string() }) {
        eprintln!("error: connection closed before send");
        return ExitCode::from(1);
    }

    // 4. wait for Delivered (and optionally echo inbound Sends)
    loop {
        match wait_event(&client) {
            Some(Event::Delivered { seq }) => {
                println!("delivered seq={seq}");
                if !listen {
                    break;
                }
            }
            Some(Event::Message { conv, from, seq, text }) => {
                eprintln!("echo: conv={} seq={} from={} msg={}", hex::encode(&conv), seq, from, text);
            }
            Some(Event::Error { msg }) => {
                eprintln!("error: {msg}");
                if !listen {
                    return ExitCode::from(1);
                }
            }
            Some(Event::Disconnected { reason }) => {
                eprintln!("disconnected: {reason}");
                return ExitCode::from(1);
            }
            Some(ev) => {
                eprintln!("incoming: {ev:?}");
                if !listen {
                    break;
                }
            }
            None => {
                if !listen {
                    eprintln!("error: timed out waiting for Delivered");
                    return ExitCode::from(1);
                }
            }
        }
    }

    // 5. clean goodbye
    let _ = client.send(Action::Goodbye);
    ExitCode::SUCCESS
}

/// block up to [`REPLY_TIMEOUT`] for the next event from the client, or None on
/// timeout. The initial `Event::Connected` is skipped.
fn wait_event(client: &Client) -> Option<Event> {
    let deadline = Instant::now() + REPLY_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match client.events().recv_timeout(remaining) {
            Ok(Event::Connected) => continue,
            Ok(ev) => return Some(ev),
            Err(_) => return None,
        }
    }
}
