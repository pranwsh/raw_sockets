//! Adapter that makes the channel-based [`msgclient::Client`] satisfy the
//! generic [`msgtui::ChatClient`] trait, so the reusable TUI frontend can
//! drive the messaging server without knowing anything about the wire protocol.

use msgclient::{Action, Client, Event};
use msgtui::{ChatClient, UiAction, UiEvent};

/// A [`msgtui::ChatClient`] backed by [`msgclient::Client`].
#[derive(Debug)]
pub struct TuiClient {
    client: Client,
}

impl TuiClient {
    /// open a connection only. Authentication happens through the TUI, which
    /// sends [`UiAction::Login`] on startup — do *not* send `Hello` here too, or
    /// the server would authenticate the same connection twice and duplicate
    /// every delivered message.
    pub fn connect(host: &str, port: u16) -> std::io::Result<Self> {
        let client = Client::connect(host, port)?;
        Ok(TuiClient { client })
    }
}

impl ChatClient for TuiClient {
    fn send(&self, action: UiAction) -> bool {
        let mapped = match action {
            UiAction::Login { user, password } => Action::Hello { user, password },
            UiAction::CreateConv { members } => Action::CreateConv { members },
            UiAction::ListConvs => Action::ListConvs,
            UiAction::Send { conv, text } => Action::Send { conv, text },
            UiAction::Ping => Action::Ping,
            UiAction::Quit => Action::Goodbye,
        };
        self.client.send(mapped)
    }

    fn poll_events(&self) -> Vec<UiEvent> {
        self.client
            .events()
            .try_iter()
            .map(map_event)
            .collect()
    }
}

fn map_event(ev: Event) -> UiEvent {
    match ev {
        Event::Connected => UiEvent::Connected,
        Event::AuthOk { created } => UiEvent::LoginOk { created },
        Event::AuthFail { reason } => UiEvent::LoginFail { reason },
        Event::ConvCreated { id } => UiEvent::ConvCreated { id },
        Event::Convs { ids } => UiEvent::Convs { ids },
        Event::Message { conv, from, seq, text } => UiEvent::Message { conv, from, seq, text },
        Event::Delivered { seq } => UiEvent::Delivered { seq },
        Event::Pong => UiEvent::Pong,
        Event::Error { msg } => UiEvent::Error { msg },
        Event::Disconnected { reason } => UiEvent::Disconnected { reason },
    }
}
