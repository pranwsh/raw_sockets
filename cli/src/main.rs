use clap::{Parser, Subcommand};

mod serve;
mod shell;
mod send;

#[derive(Parser)]
#[command(name = "msgcli", about = "Messaging CLI — operate the server or connect as a client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the messaging server
    Serve {
        #[arg(long, default_value = "0.0.0.0:9723")]
        bind: String,
        #[arg(long, default_value = "/tmp/msgd.redb")]
        data: String,
    },
    /// Interactive client shell (REPL)
    Shell {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 9723)]
        port: u16,
    },
    /// One-shot: connect, auth, send a message, and exit
    Send {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 9723)]
        port: u16,
        #[arg(long)]
        user: String,
        #[arg(long, default_value_t = false)]
        listen: bool,
        #[arg(long)]
        conv: Option<String>,
        #[arg(long)]
        create: Option<String>,
        #[arg(long)]
        token: Option<String>,
        message: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve { bind, data } => {
            serve::run(&bind, &data);
        }
        Command::Shell { host, port } => {
            shell::run(&host, port);
        }
        Command::Send { host, port, user, conv, create, token, message, listen } => {
            if conv.is_none() && create.is_none() {
                eprintln!("error: either --conv or --create is required");
                std::process::exit(1);
            }
            send::run(&host, port, &user, conv.as_deref(), create.as_deref(), token.as_deref(), &message, listen);
        }
    }
}
