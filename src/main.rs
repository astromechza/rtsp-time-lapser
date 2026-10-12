// Modules are added task by task. `dead_code` is allowed until Task 6 wires everything into main.
#![allow(dead_code)]

mod capture;
mod config;
mod ffmpeg;
mod metrics;
mod store;

use clap::Parser;

fn main() {
    let cfg = config::Config::parse();
    println!("{cfg:?}");
}
