use clap::{Parser, Subcommand};
use std::process::ExitCode;

mod serve;
mod send;
mod shell;
mod tui_client;

#[derive(Parser)]
#[command(name = "msgcli", about = "Messaging CLI — operate the server or connect as a client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// start the messaging server
    Serve {
        #[arg(long, default_value = "0.0.0.0:9723")]
        bind: String,
        #[arg(long, default_value = "/tmp/msgd.redb")]
        data: String,
    },
    /// interactive TUI chat client (term_render frontend)
    Shell {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 9723)]
        port: u16,
        #[arg(long, default_value = "")]
        user: String,
        #[arg(long, default_value = "")]
        password: String,
        #[arg(long)]
        conv: Option<String>,
    },
    /// one-shot: connect, auth, send a message, and exit
    Send {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 9723)]
        port: u16,
        #[arg(long)]
        user: String,
        #[arg(long)]
        password: String,
        #[arg(long, default_value_t = false)]
        listen: bool,
        #[arg(long)]
        conv: Option<String>,
        #[arg(long)]
        create: Option<String>,
        message: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { bind, data } => {
            serve::run(&bind, &data);
            ExitCode::SUCCESS
        }
        Command::Shell { host, port, user, password, conv } => {
            shell::run(&host, port, &user, &password, conv.as_deref())
        }
        Command::Send { host, port, user, password, conv, create, message, listen } => {
            if conv.is_none() && create.is_none() {
                eprintln!("error: either --conv or --create is required");
                return ExitCode::from(1);
            }
            send::run(&host, port, &user, &password, conv.as_deref(), create.as_deref(), &message, listen);
            ExitCode::SUCCESS
        }
    }
}
