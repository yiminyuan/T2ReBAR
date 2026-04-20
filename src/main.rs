mod config;
mod diagnose;
mod execute;
mod manifest;
mod pci;
mod plan;
mod preflight;
mod rebar;
mod topology;

use std::path::PathBuf;
use std::process::ExitCode;

use execute::Options;
use plan::Mode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("diagnose");
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();

    let result = match cmd {
        "diagnose" => diagnose::run(),
        "plan" => run_plan(&rest),
        "execute" => run_execute(&rest),
        "rollback" => run_rollback(&rest),
        "verify" => run_verify(&rest),
        "-h" | "--help" | "help" => {
            print_help();
            return ExitCode::SUCCESS;
        }
        other => {
            eprintln!("unknown subcommand: {other}");
            print_help();
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            let mut src = e.source();
            while let Some(cause) = src {
                eprintln!("  caused by: {cause}");
                src = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

fn run_plan(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let (mode, _opts) = parse_flags(args)?;
    let p = plan::build(mode)?;
    plan::print(&p);
    Ok(())
}

fn run_execute(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let (mode, opts) = parse_flags(args)?;
    let p = plan::build(mode)?;
    execute::execute(&p, opts)
}

fn run_rollback(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let (_mode, opts) = parse_flags(args)?;
    execute::rollback(opts)
}

fn run_verify(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let (_mode, opts) = parse_flags(args)?;
    execute::verify_cmd(opts)
}

fn parse_flags(args: &[&str]) -> Result<(Mode, Options), Box<dyn std::error::Error>> {
    let mut mode = Mode::Planned;
    let mut yes = false;
    let mut force = false;
    let mut manifest_path: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        match a {
            "--yes" | "-y" => yes = true,
            "--force" => force = true,
            "--target" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or("--target requires a value")?;
                mode = parse_target(v)?;
            }
            t if t.starts_with("--target=") => {
                mode = parse_target(&t["--target=".len()..])?;
            }
            "--manifest" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or("--manifest requires a value")?;
                manifest_path = Some(PathBuf::from(v));
            }
            t if t.starts_with("--manifest=") => {
                manifest_path = Some(PathBuf::from(&t["--manifest=".len()..]));
            }
            other => {
                return Err(format!("unknown flag: {other}").into());
            }
        }
        i += 1;
    }

    Ok((
        mode,
        Options {
            yes,
            force,
            manifest_path,
        },
    ))
}

fn parse_target(v: &str) -> Result<Mode, Box<dyn std::error::Error>> {
    match v {
        "planned" => Ok(Mode::Planned),
        "current" => Ok(Mode::Current),
        other => {
            if let Ok(n) = other.parse::<u8>() {
                Ok(Mode::Explicit(n))
            } else {
                Err(format!("bad --target value: {other}").into())
            }
        }
    }
}

fn print_help() {
    println!(
        "t2rebar — PCIe BAR resize tool for Mac Pro 2019

USAGE:
  t2rebar diagnose                       Read-only report on GPUs + rebar
  t2rebar plan [--target=<t>]            Build and print an action plan
  t2rebar execute [flags]                Run the plan (DESTRUCTIVE)
  t2rebar rollback [flags]               Restore original sizes from manifest
  t2rebar verify [flags]                 Compare live state vs manifest
  t2rebar help                           This message

FLAGS:
  --target=planned    (default) Use hard-coded preferred size per GPU
  --target=current    Dry-cycle: unbind/remove/rescan/rebind without resizing
  --target=<N>        Explicit size index (advanced; all GPUs same size)
  --yes, -y           Skip interactive confirmation
  --force             Bypass safety refusals (pci=realloc, active consumers)
  --manifest=<path>   Override manifest path (default /var/lib/t2rebar/state.txt)

EXAMPLES:
  sudo t2rebar diagnose
  sudo t2rebar plan --target=current
  sudo t2rebar execute --target=current --yes
  sudo t2rebar rollback --yes
"
    );
}
