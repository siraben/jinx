# Known divergences from C++ Nix

jinx is **byte-identical to C++ Nix** on:
- the full language conformance corpus (all 467 `tests/functional/lang` fixtures, incl.
  stdout, stderr, and `--show-trace` traces), verified under `JINX_GC_STRESS=1` and
  `JINX_JIT=1 JINX_JIT_THRESHOLD=0`;
- derivation outputs — `.drv` files and store paths — across nixpkgs (`hello`, `firefox`,
  the full minimal-ISO closure, and large samples) and a broad matrix of synthetic edge
  cases (structured attrs, multi-output, fixed-output across all hash algos/modes, etc.).

Outside those, the following observable differences are **known and intentional** (either
an upstream quirk we choose not to replicate, a case where jinx is arguably more correct,
or an out-of-scope subsystem). Cases that were *bugs* have been fixed; this file lists
what remains by design. Established by adversarial differential review, 2026-07.

## Upstream quirks jinx does not replicate (jinx is arguably more correct)

- **`builtins.unsafeGetAttrPos` on inline attrs in the directly-`--eval`'d file.** For an
  attribute set written inline in the top-level file, C++ reports `line = 1` with the
  column equal to the attribute's flat byte offset (a position-recording artifact). jinx
  reports the true line/column of the attribute name. jinx matches C++ exactly for attrs
  in *imported* files and for stdin. Replicating the artifact would jeopardize jinx's
  byte-exact trace positions (which pass all 467 fixtures), so we don't.

- **Regex catastrophic backtracking.** `builtins.match "(a+)+$" "aaa…!"` — C++'s
  `std::regex` aborts with a complexity-limit error; jinx's own POSIX-ERE engine (a linear
  PikeVM) matches without exhausting resources and returns `null`. jinx is more correct;
  the two differ in output and exit code only for pathological patterns.

## Platform-specific rendering

- **Error source-excerpt on a non-canonical absolute path (macOS).** When a file is
  passed by an absolute path that traverses a symlink differently than Nix's accessor
  canonicalizes it (e.g. `/tmp/x.nix` vs `/private/tmp/x.nix` on macOS), C++ fails to
  re-read the file at error-render time and silently omits the source excerpt under the
  position; jinx reads it by the given path and prints the excerpt. Relative paths and all
  real-world workloads (which use canonical paths) match byte-for-byte.

## Out-of-scope subsystems (jinx is an evaluator, not a builder/store)

- **`builtins.fetchGit` / `builtins.storePath`** are not implemented (they error). Flake
  inputs of type `path` and `git+file` *are* supported (used for `nix eval <flake>#…`);
  `github:`/tarball/network-git fetchers are not.
- **Flake lock generation (`lockFlake`)** is not implemented: an on-disk `flake.lock`
  (or a lock-free single flake) is required. Flakes needing in-memory lock computation
  (uncommitted inputs, `nix flake lock`) are unsupported.
- **Readonly-mode store-validity semantics.** Under `--readonly-mode` / `dummy://`, jinx
  checks the filesystem where C++ checks store-path *validity*. Consequently some
  errors that C++ raises about not-yet-built store paths (e.g. certain import-from-
  derivation shapes, `readFile`/`hashFile` on unrealised paths) do not fire identically
  in jinx's readonly mode. With a real daemon store, jinx performs genuine builds (IFD) and
  writes byte-identical `.drv` files.
- **`nix build`-style scheduling, substituters, the daemon *server*, `nix repl`, and the
  debugger** are out of scope. Builds happen via the real `nix-daemon`.

## Library-driven error wording

- **`builtins.fromJSON` / `fromTOML` parse-error text.** jinx's messages come from the
  Rust `serde_json` / `toml` parsers; C++'s come from `nlohmann/json` / `toml11`. The
  *values* parse identically (including integer-vs-float classification); only the text
  of a *parse-failure* message differs.

## Resource note

- Each `path`/`git+file` flake fetch extracts the source tree to a temp directory that
  currently persists for the process lifetime (the virtual-store redirect reads from it).
  Large flakes (e.g. nixpkgs, ~1.5 GB extracted) therefore consume temp space per eval.
