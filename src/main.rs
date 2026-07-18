use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use north_db::Config;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("north: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args_os().skip(1);
    let mut config_path = PathBuf::from("north.yaml");

    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("--config") => {
                config_path = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| "--config requires a path".to_owned())?;
            }
            Some("--help" | "-h") => {
                println!("Usage: north [--config <path>]");
                return Ok(());
            }
            Some(other) => return Err(format!("unknown argument: {other}")),
            None => return Err("arguments must be valid UTF-8".to_owned()),
        }
    }

    let config = Config::load(&config_path).map_err(|error| error.to_string())?;
    println!(
        "North configuration is valid (database: {})",
        config.storage.path.display()
    );
    Ok(())
}
