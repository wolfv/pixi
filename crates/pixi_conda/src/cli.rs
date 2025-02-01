use crate::create;
use clap::Subcommand;
use pixi_config::Config;

/// Pixi-conda is a tool for managing conda environments.
#[derive(Subcommand, Debug)]
pub enum Args {
    Create(create::Args),
}

pub async fn execute(args: Args) -> miette::Result<()> {
    let config = Config::load_global();

    match args {
        Args::Create(args) => create::execute(config, args).await,
    }
}
