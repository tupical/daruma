//! `taskagent-desktop` — local-first CLI client.
//!
//! Embeds the same engine (storage + core + ai) that the server uses, so
//! it works fully offline. A GPUI graphical client will land in a future
//! revision; the `gpui` dependency is intentionally commented out in this
//! crate's `Cargo.toml` (see docs/ARCHITECTURE.md).
//!
//! Subcommands (all idempotent, all event-sourced):
//!
//! ```text
//! taskagent list            [inbox|todo|in_progress|done]
//! taskagent add  "<title>"  [--p0|--p1|--p2|--p3]
//! taskagent done <id|prefix>
//! taskagent delete <id|prefix>
//! taskagent ai parse "<input>"
//! taskagent ai decompose <id|prefix>
//! ```

mod cmds;
mod context;
mod flush;
mod local_executor;
mod outbox;
mod remote;
mod render;
mod replica;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let mut args = std::env::args().skip(1);
    let Some(sub) = args.next() else {
        print_help();
        return Ok(());
    };
    let rest: Vec<String> = args.collect();

    match sub.as_str() {
        "list" | "ls" => {
            let ctx = context::Context::open().await?;
            cmds::list(&ctx, &rest).await?;
        }
        "add" | "new" => {
            let ctx = context::Context::open().await?;
            cmds::add(&ctx, &rest).await?;
        }
        "done" | "complete" => {
            let ctx = context::Context::open().await?;
            cmds::done(&ctx, &rest).await?;
        }
        "delete" | "rm" => {
            let ctx = context::Context::open().await?;
            cmds::delete(&ctx, &rest).await?;
        }
        "ai" => {
            let ctx = context::Context::open().await?;
            cmds::ai(&ctx, &rest).await?;
        }
        "sync" => {
            let ctx = context::Context::open().await?;
            cmds::sync(&ctx, &rest).await?;
        }
        "where" => {
            // Print the resolved DB path for debugging.
            let path = context::data_path();
            println!("{}", path.display());
        }
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("unknown subcommand: {other}\n");
            print_help();
            std::process::exit(2);
        }
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,taskagent_desktop=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn print_help() {
    println!(
        "taskagent — local-first task runtime\n\n\
         USAGE\n  \
         taskagent <subcommand> [args]\n\n\
         SUBCOMMANDS\n  \
         list [inbox|todo|in_progress|done]   show tasks\n  \
         add  \"<title>\" [--p0..--p3]          create a task\n  \
         done <id|prefix>                     mark complete\n  \
         delete <id|prefix>                   delete a task\n  \
         sync [--limit N]                     flush offline events to server\n  \
         ai parse \"<input>\"                   parse natural language → CreateTask\n  \
         ai decompose <id|prefix>             split a task into subtasks\n  \
         where                                print the DB path\n  \
         help                                 this message\n\n\
         ENV\n  \
         TASKAGENT_DATA_DIR   directory for replica.sqlite (default: `.`)\n  \
         TASKAGENT_API_URL    server base for `sync` (default: http://localhost:8080)\n  \
         TASKAGENT_TOKEN      bearer token for `sync`\n  \
         OPENAI_API_KEY       required for `ai *` subcommands\n  \
         OPENAI_MODEL         model id (default: gpt-4.1-mini)\n  \
         RUST_LOG             tracing filter\n"
    );
}
