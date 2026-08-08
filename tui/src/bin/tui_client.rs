//! Adapter that makes the channel-based [`msgclient::Client`] satisfy the
//! generic [`msgtui::ChatClient`] trait, so the reusable TUI frontend can
//! drive the messaging server. Both sides speak the shared
//! [`chat_model::Action`]/[`chat_model::Event`] model, so this is a thin
//! forwarder.
//!
//! This lives under `src/bin/` so the `msgtui` lib stays free of any wire
//! knowledge — only the `msgtui` binary ties the protocol to the frontend.

use msgclient::{Action, Client, Event};
use msgtui::ChatClient;

/// A [`msgtui::ChatClient`] backed by [`msgclient::Client`].
#[derive(Debug)]
pub struct TuiClient {
    client: Client,
}

impl TuiClient {
    /// open a connection only. Authentication happens through the TUI, which
    /// sends [`Action::Hello`] on startup — do *not* send `Hello` here too, or
    /// the server would authenticate the same connection twice and duplicate
    /// every delivered message.
    pub fn connect(host: &str, port: u16) -> std::io::Result<Self> {
        let client = Client::connect(host, port)?;
        Ok(TuiClient { client })
    }
}

impl ChatClient for TuiClient {
    fn send(&self, action: Action) -> bool {
        self.client.send(action)
    }

    fn poll_events(&self) -> Vec<Event> {
        self.client.events().try_iter().collect()
    }
}
