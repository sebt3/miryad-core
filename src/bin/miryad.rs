use clap::Parser;

/// CLI de scaffolding miryad — génère une application depuis un modèle de données.
#[derive(Parser)]
#[command(name = "miryad", version)]
struct Cli;

fn main() {
    Cli::parse();
}
