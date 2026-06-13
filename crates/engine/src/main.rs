//! `otto run "<goal>" [--root <path>]` — run a single turn and print the event stream.

use std::path::PathBuf;

use otto_engine::{build_router, run_goal};
use otto_workspace::LocalWorkspace;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    if command != "run" {
        eprintln!("usage: otto run \"<goal>\" [--root <path>]");
        std::process::exit(2);
    }

    let goal = args.next().unwrap_or_else(|| {
        eprintln!("error: missing goal");
        std::process::exit(2);
    });

    let mut root = PathBuf::from(".");
    if let Some(flag) = args.next() {
        if flag == "--root" {
            match args.next() {
                Some(path) => root = PathBuf::from(path),
                None => {
                    eprintln!("error: --root requires a path");
                    std::process::exit(2);
                }
            }
        }
    }

    let router = build_router();
    let workspace = LocalWorkspace::new(root);

    let (events, outcome) = run_goal(&goal, router.as_ref(), &workspace).await?;
    for event in &events {
        println!("[{:>3}] {:?}", event.seq, event.kind);
    }
    println!("turn ok = {}", outcome.ok);

    if !outcome.ok {
        std::process::exit(1);
    }
    Ok(())
}
