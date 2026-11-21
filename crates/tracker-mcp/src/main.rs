//! Бинарник `tracker-mcp`: CLI и транспорт MCP-сервера Yandex Tracker.
//!
//! `main` только разбирает аргументы и вызывает модуль подкоманды из [`cli`].

mod browser;
mod cli;
mod clients;
mod login;
mod oauth;
mod profiles;
mod ui;
mod verify;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// MCP server and CLI for the Yandex Tracker REST API.
#[derive(Debug, Parser)]
#[command(name = "tracker-mcp", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the MCP server over stdio with the access of the profile (read-only by default), or
    /// over HTTP with --http.
    Serve(cli::serve::ServeArgs),
    /// Manage stored tokens and Tracker organizations of profiles.
    #[command(subcommand)]
    Auth(cli::auth::AuthCommand),
    /// Register the server in an MCP client (Claude Code or Codex) for a profile.
    ///
    /// Sets up your Yandex OAuth application, the organization and the token of the profile
    /// (or, with --iam, Yandex Cloud IAM tokens from yc), then adds
    /// `tracker-mcp serve --profile <profile>` to the client config after confirmation. The
    /// token never goes to the client config.
    Connect(cli::connect::ConnectArgs),
    /// List and check custom tools defined in tools.toml.
    #[command(subcommand)]
    Tools(cli::tools::ToolsCommand),
    /// Call a tool of the server from the console, with the access of the profile; without
    /// a tool, list the tools.
    Call(cli::call::CallCommand),
    /// Interactive view of the profiles: settings, access and its approval, registrations
    /// in MCP clients, the token's Tracker user, and the tools.
    Ui(cli::tui::UiArgs),
    /// Internal: run one `jq` filter for `serve` (JSON on stdin and stdout).
    #[command(name = "jq-worker", hide = true)]
    JqWorker,
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    // Спецификация встроена в бинарник; ссылка на неё не даёт линковщику её выбросить.
    let _ = tracker_mcp_core::tracker_api::OPENAPI_JSON.len();
    match Cli::parse().command {
        Command::Serve(args) => cli::serve::run(args).await.map(|()| ExitCode::SUCCESS),
        Command::Auth(command) => cli::auth::run(command).await.map(|()| ExitCode::SUCCESS),
        Command::Connect(args) => cli::connect::run(args).await.map(|()| ExitCode::SUCCESS),
        Command::Tools(command) => cli::tools::run(command).map(|()| ExitCode::SUCCESS),
        // Код завершения `call` — 1, если инструмент вернул ошибку.
        Command::Call(args) => cli::call::run(args).await,
        Command::Ui(args) => cli::tui::run(args).await.map(|()| ExitCode::SUCCESS),
        Command::JqWorker => cli::jq_worker::run().map(|()| ExitCode::SUCCESS),
    }
}
