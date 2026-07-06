//! jinx: nix-instantiate-compatible CLI. Milestone M1 implements only
//! `--parse -` (parse stdin, print `Expr::show`, exit 0; on error print the
//! Nix-formatted error to stderr and exit 1).

use std::io::{Read, Write};
use std::process::ExitCode;

use jinx_syntax::{parse_and_bind, show, Origin, PosTable};

struct Options {
    parse_only: bool,
    read_stdin: bool,
    files: Vec<String>,
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options {
        parse_only: false,
        read_stdin: false,
        files: vec![],
    };
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--parse" | "--parse-only" => opts.parse_only = true,
            "-" => opts.read_stdin = true,
            s if s.starts_with('-') => {
                // Other nix-instantiate flags arrive in later milestones.
                return Err(format!("unsupported argument '{s}'"));
            }
            s => opts.files.push(s.to_string()),
        }
    }
    Ok(opts)
}

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    if !opts.parse_only || !opts.read_stdin || !opts.files.is_empty() {
        eprintln!("error: only '--parse -' is supported in this milestone");
        return ExitCode::FAILURE;
    }

    let mut source = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut source) {
        eprintln!("error: reading stdin: {e}");
        return ExitCode::FAILURE;
    }

    let base_path = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into());
    let home = std::env::var("HOME").ok();

    let mut positions = PosTable::new();
    let origin = Origin::Stdin {
        source: source.clone(),
    };
    let mut warnings = Vec::new();
    let result = parse_and_bind(
        &source,
        origin,
        &base_path,
        home.as_deref(),
        &mut positions,
        &mut warnings,
    );
    for w in &warnings {
        eprintln!("{w}");
    }
    match result {
        Ok(res) => {
            let mut out = show::show(&res.exprs, &res.symbols, res.root);
            out.push(b'\n');
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = lock.write_all(&out);
            let _ = lock.flush();
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{}", e.render(&positions));
            ExitCode::FAILURE
        }
    }
}
