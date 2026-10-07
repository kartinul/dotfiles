use clap::Parser;

use xvpn::cli::{dispatch, Cli};

fn main() {
    dispatch(Cli::parse());
}
