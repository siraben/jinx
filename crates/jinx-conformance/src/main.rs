//! Conformance runner for the Nix language test suite
//! (`/path/to/nix/tests/functional/lang`), replicating `lang.sh` semantics
//! exactly: same env, same flags handling, same pwd-substitution, same
//! missing-expected-file-means-empty rule, same `.postprocess` hooks.
//!
//! The engine under test is any `nix-instantiate`-compatible binary
//! (the C++ oracle for harness validation, jinx for conformance).

use clap::Parser;
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Parser)]
struct Args {
    /// Path to the tests/functional directory (corpus root; tests live in lang/)
    #[arg(long, default_value = "$NIX_SRC/tests/functional")]
    corpus: PathBuf,
    /// nix-instantiate-compatible binary under test
    #[arg(long)]
    engine: PathBuf,
    /// Only run tests whose name contains this substring (or glob-ish prefix)
    #[arg(long)]
    filter: Option<String>,
    /// Print unified diffs for mismatches
    #[arg(long)]
    diff: bool,
    /// Only print the summary line
    #[arg(short, long)]
    quiet: bool,
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    ParseFail,
    ParseOkay,
    EvalFail,
    EvalOkay,
}

struct Outcome {
    name: String,
    ok: bool,
    skipped: bool,
    detail: String,
}

fn main() {
    let mut args = Args::parse();
    // The engine is spawned with cwd=corpus; resolve it before that.
    args.engine = args.engine.canonicalize().expect("engine binary");
    let corpus = args.corpus.canonicalize().expect("corpus dir");
    let lang = corpus.join("lang");

    // One-time test environment, mirroring common/vars.sh + init.sh + lang.sh.
    let test_root = std::env::temp_dir().join(format!("jinx-conformance-{}", std::process::id()));
    let _ = fs::remove_dir_all(&test_root);
    fs::create_dir_all(test_root.join("etc")).unwrap();
    fs::create_dir_all(test_root.join("test-home")).unwrap();
    fs::write(
        test_root.join("etc/nix.conf"),
        format!(
            "build-users-group =\nkeep-derivations = false\nsandbox = false\n\
             experimental-features = nix-command\ngc-reserved-space = 0\n\
             substituters =\nflake-registry = {tr}/registry.json\nshow-trace = true\n\
             include nix.conf.extra\n",
            tr = test_root.display()
        ),
    )
    .unwrap();
    fs::write(
        test_root.join("etc/nix.conf.extra"),
        "fsync-metadata = false\nextra-experimental-features = flakes\n!include nix.conf.extra.not-there\n",
    )
    .unwrap();

    let mut tests: Vec<(Kind, String)> = vec![];
    let mut entries: Vec<_> = fs::read_dir(&lang)
        .expect("lang dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    for f in &entries {
        let Some(base) = f.strip_suffix(".nix") else { continue };
        let kind = if base.starts_with("parse-fail-") {
            Kind::ParseFail
        } else if base.starts_with("parse-okay-") {
            Kind::ParseOkay
        } else if base.starts_with("eval-fail-") {
            Kind::EvalFail
        } else if base.starts_with("eval-okay-") {
            Kind::EvalOkay
        } else {
            continue;
        };
        if let Some(filt) = &args.filter {
            let pat = filt.trim_end_matches('*');
            if !base.starts_with(pat) && !base.contains(filt.as_str()) {
                continue;
            }
        }
        tests.push((kind, base.to_string()));
    }

    let outcomes: Vec<Outcome> = tests
        .par_iter()
        .map(|(kind, name)| run_test(&args, &corpus, &test_root, *kind, name))
        .collect();

    let mut pass = 0;
    let mut fail = 0;
    let mut skip = 0;
    for o in &outcomes {
        if o.skipped {
            skip += 1;
        } else if o.ok {
            pass += 1;
        } else {
            fail += 1;
            if !args.quiet {
                println!("FAIL {}", o.name);
                if args.diff {
                    println!("{}", o.detail);
                }
            }
        }
    }
    println!(
        "conformance: {pass} passed, {fail} failed, {skip} skipped (of {})",
        outcomes.len()
    );
    let _ = fs::remove_dir_all(&test_root);
    std::process::exit(if fail > 0 { 1 } else { 0 });
}

fn base_env(cmd: &mut Command, corpus: &Path, test_root: &Path) {
    use std::os::unix::process::CommandExt;
    // lang.sh finds the binary via PATH, so it sees argv[0]="nix-instantiate";
    // usage errors embed argv[0], and jinx dispatches CLI personality on it.
    cmd.arg0("nix-instantiate");
    cmd.env_clear();
    // PATH kept minimal; .postprocess runs under bash separately.
    cmd.env("PATH", "/usr/bin:/bin");
    cmd.env("NIX_CONF_DIR", test_root.join("etc"));
    cmd.env("NIX_STORE_DIR", "/nix/store");
    cmd.env("NIX_REMOTE", "dummy://");
    cmd.env("NIX_LOCALSTATE_DIR", test_root.join("var"));
    cmd.env("NIX_LOG_DIR", test_root.join("var/log/nix"));
    cmd.env("NIX_STATE_DIR", test_root.join("var/nix"));
    cmd.env("TEST_VAR", "foo");
    cmd.env("PAGER", "cat");
    cmd.env("HOME", test_root.join("test-home"));
    cmd.env("_NIX_TEST_NO_ENVIRONMENT_WARNINGS", "1");
    cmd.env("_NIX_IN_TEST", test_root.join("shared"));
    cmd.current_dir(corpus);
}

fn run_test(args: &Args, corpus: &Path, test_root: &Path, kind: Kind, name: &str) -> Outcome {
    let lang = corpus.join("lang");
    let nix_file = lang.join(format!("{name}.nix"));
    let pwd = corpus.to_string_lossy().into_owned();

    let read_expected = |suffix: &str| -> Vec<u8> {
        fs::read(lang.join(format!("{name}.{suffix}"))).unwrap_or_default()
    };

    let mk_fail = |detail: String| Outcome {
        name: name.to_string(),
        ok: false,
        skipped: false,
        detail,
    };

    match kind {
        Kind::ParseFail => {
            let mut cmd = Command::new(&args.engine);
            base_env(&mut cmd, corpus, test_root);
            cmd.args(["--parse", "-"]);
            cmd.stdin(Stdio::from(fs::File::open(&nix_file).unwrap()));
            let out = cmd.output().expect("spawn engine");
            if out.status.code() != Some(1) {
                return mk_fail(format!("expected exit 1, got {:?}", out.status.code()));
            }
            let got = postprocess(corpus, name, "err", out.stderr.clone());
            compare(args, name, &got, &read_expected("err.exp"), "err")
        }
        Kind::ParseOkay => {
            let mut cmd = Command::new(&args.engine);
            base_env(&mut cmd, corpus, test_root);
            cmd.args(["--parse", "-"]);
            cmd.stdin(Stdio::from(fs::File::open(&nix_file).unwrap()));
            let out = cmd.output().expect("spawn engine");
            if out.status.code() != Some(0) {
                return mk_fail(format!(
                    "expected exit 0, got {:?}\nstderr: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            // NOTE: lang.sh runs sed *without* -i for parse-okay: compared raw.
            let stdout = postprocess(corpus, name, "out", out.stdout.clone());
            let o1 = compare(args, name, &stdout, &read_expected("exp"), "out");
            if !o1.ok {
                return o1;
            }
            compare(args, name, &out.stderr, &read_expected("err.exp"), "err")
        }
        Kind::EvalFail => {
            // Flags: .flags with #-comments stripped, whitespace-split;
            // default --eval --strict --show-trace.
            let flags_file = lang.join(format!("{name}.flags"));
            let flags: Vec<String> = if flags_file.exists() {
                let s = fs::read_to_string(&flags_file).unwrap();
                s.lines()
                    .map(|l| l.split('#').next().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join(" ")
                    .split_whitespace()
                    .map(String::from)
                    .collect()
            } else {
                vec!["--eval".into(), "--strict".into(), "--show-trace".into()]
            };
            let mut cmd = Command::new(&args.engine);
            base_env(&mut cmd, corpus, test_root);
            cmd.args(&flags).arg(format!("lang/{name}.nix"));
            let out = cmd.output().expect("spawn engine");
            if out.status.code() != Some(1) {
                return mk_fail(format!(
                    "expected exit 1, got {:?}\nstderr: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            let got = subst_pwd(&out.stderr, &pwd);
            let got = postprocess(corpus, name, "err", got);
            compare(args, name, &got, &read_expected("err.exp"), "err")
        }
        Kind::EvalOkay => {
            if lang.join(format!("{name}.exp.xml")).exists() {
                let mut cmd = Command::new(&args.engine);
                base_env(&mut cmd, corpus, test_root);
                cmd.args(["--eval", "--xml", "--no-location", "--strict"])
                    .arg(format!("lang/{name}.nix"));
                let out = cmd.output().expect("spawn engine");
                if out.status.code() != Some(0) {
                    return mk_fail(format!(
                        "expected exit 0, got {:?}\nstderr: {}",
                        out.status.code(),
                        String::from_utf8_lossy(&out.stderr)
                    ));
                }
                let got = postprocess(corpus, name, "out.xml", out.stdout.clone());
                return compare(args, name, &got, &read_expected("exp.xml"), "out.xml");
            }
            if lang.join(format!("{name}.exp-disabled")).exists() {
                return Outcome {
                    name: name.to_string(),
                    ok: true,
                    skipped: true,
                    detail: String::new(),
                };
            }
            // Flags: first line of .flags, whitespace-split (bash `read -r -a`).
            let flags_file = lang.join(format!("{name}.flags"));
            let flags: Vec<String> = if flags_file.exists() {
                fs::read_to_string(&flags_file)
                    .unwrap()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .map(String::from)
                    .collect()
            } else {
                vec![]
            };
            let mut cmd = Command::new(&args.engine);
            base_env(&mut cmd, corpus, test_root);
            cmd.env("NIX_PATH", "lang/dir3:lang/dir4");
            cmd.env("HOME", "/fake-home");
            cmd.args(&flags)
                .args(["--eval", "--strict"])
                .arg(format!("lang/{name}.nix"));
            let out = cmd.output().expect("spawn engine");
            if out.status.code() != Some(0) {
                return mk_fail(format!(
                    "expected exit 0, got {:?}\nstderr: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
            let stdout = subst_pwd(&out.stdout, &pwd);
            let stderr = subst_pwd(&out.stderr, &pwd);
            let stdout = postprocess(corpus, name, "out", stdout);
            let o1 = compare(args, name, &stdout, &read_expected("exp"), "out");
            if !o1.ok {
                return o1;
            }
            compare(args, name, &stderr, &read_expected("err.exp"), "err")
        }
    }
}

/// sed "s!$(pwd)!/pwd!g"
fn subst_pwd(bytes: &[u8], pwd: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let needle = pwd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(needle) {
            out.extend_from_slice(b"/pwd");
            i += needle.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// Run lang/<name>.postprocess (bash) against the got-output, like lang.sh does.
/// `ext` is the stream's file extension ("out", "err", "out.xml") — the script
/// edits "$prefix.<ext>" in place.
fn postprocess(corpus: &Path, name: &str, ext: &str, got: Vec<u8>) -> Vec<u8> {
    let script = corpus.join("lang").join(format!("{name}.postprocess"));
    if !script.exists() {
        return got;
    }
    let dir = std::env::temp_dir().join(format!("jinx-pp-{}-{name}-{ext}", std::process::id()));
    let _ = fs::create_dir_all(&dir);
    let prefix = dir.join(name);
    // Write all streams the script might touch; the one we care about is `ext`.
    for e in ["out", "err", "out.xml"] {
        fs::write(format!("{}.{e}", prefix.display()), &got).unwrap();
    }
    let st = Command::new("bash")
        .arg(&script)
        .arg(&prefix)
        .current_dir(corpus)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run postprocess");
    assert!(st.success(), "postprocess failed for {name}");
    let r = fs::read(format!("{}.{ext}", prefix.display())).unwrap();
    let _ = fs::remove_dir_all(&dir);
    r
}

fn compare(args: &Args, name: &str, got: &[u8], expected: &[u8], which: &str) -> Outcome {
    let ok = got == expected;
    let detail = if !ok && args.diff {
        format!(
            "--- expected {name}.{which} / +++ got\n{}",
            simple_diff(expected, got)
        )
    } else {
        String::new()
    };
    Outcome {
        name: format!("{name} ({which})"),
        ok,
        skipped: false,
        detail,
    }
}

fn simple_diff(a: &[u8], b: &[u8]) -> String {
    let a = String::from_utf8_lossy(a);
    let b = String::from_utf8_lossy(b);
    let al: Vec<&str> = a.lines().collect();
    let bl: Vec<&str> = b.lines().collect();
    let mut out = String::new();
    let n = al.len().max(bl.len());
    for i in 0..n {
        let (x, y) = (al.get(i), bl.get(i));
        if x != y {
            if let Some(x) = x {
                out.push_str(&format!("-{x}\n"));
            }
            if let Some(y) = y {
                out.push_str(&format!("+{y}\n"));
            }
        }
    }
    out
}
