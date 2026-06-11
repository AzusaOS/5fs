use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

/// Check a 5FS filesystem (read-only, structural).
#[derive(Parser)]
#[command(name = "fsck.5fs", version)]
struct Args {
    /// Image file or block device
    device: PathBuf,
    /// Only report errors
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let report = match gofs::fsck::check(&args.device) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fsck.5fs: {e:#}");
            return ExitCode::from(8); // operational error
        }
    };
    if !args.quiet {
        for m in &report.info {
            println!("  {m}");
        }
    }
    for m in &report.warnings {
        println!("WARN  {m}");
    }
    for m in &report.errors {
        println!("ERROR {m}");
    }
    if report.clean() {
        if !args.quiet {
            println!("{}: clean", args.device.display());
        }
        ExitCode::SUCCESS
    } else {
        println!("{}: {} error(s)", args.device.display(), report.errors.len());
        ExitCode::from(4) // fsck convention: uncorrected errors
    }
}
