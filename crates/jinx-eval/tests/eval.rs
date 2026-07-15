//! End-to-end evaluator tests: parse -> compile -> run -> deep-force ->
//! printAmbiguous, comparing against nix-instantiate --eval --strict output.

use jinx_eval::builtins;
use jinx_eval::print;
use jinx_eval::vm::VM;
use jinx_syntax::{Origin, PosTable, SymbolTable};

fn eval(expr: &str) -> Result<String, String> {
    // Deep recursion (max-call-depth 10000) needs a large native stack, and
    // the heap's conservative scanner must be created on the running thread.
    let expr = expr.to_string();
    std::thread::Builder::new()
        .stack_size(1 << 29)
        .spawn(move || eval_inner(&expr))
        .unwrap()
        .join()
        .unwrap()
}

fn eval_inner(expr: &str) -> Result<String, String> {
    let symbols = SymbolTable::new();
    let positions = PosTable::new();
    let mut vm = VM::new(symbols, positions);
    builtins::register_globals(&mut vm);
    let source = expr.as_bytes().to_vec();
    let mut warnings = Vec::new();
    let (exprs, root) = jinx_syntax::parse_and_bind_with(
        &source,
        Origin::String {
            source: source.clone(),
        },
        "/test",
        None,
        &mut vm.positions,
        &mut vm.symbols,
        &mut warnings,
    )
    .map_err(|e| String::from_utf8_lossy(&e.msg).into_owned())?;
    let prog = jinx_eval::compile::compile_program(
        &exprs,
        root,
        &vm.symbols,
        &vm.globals,
        vm.empty_list_cell,
    );
    let cell = vm
        .run_program(prog)
        .and_then(|c| print::deep_force(&mut vm, c).map(|_| c))
        .map_err(|e| String::from_utf8_lossy(&vm.errors[e as usize].msg).into_owned())?;
    let mut out = Vec::new();
    print::print_ambiguous(&mut vm, cell, &mut out)
        .map_err(|e| String::from_utf8_lossy(&vm.errors[e as usize].msg).into_owned())?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn ok(expr: &str, expected: &str) {
    assert_eq!(eval(expr).expect(expr), expected, "expr: {expr}");
}

fn fails(expr: &str, msg_part: &str) {
    let e = eval(expr).expect_err(expr);
    assert!(e.contains(msg_part), "expr: {expr}: got error {e:?}");
}

#[test]
fn deep_seq_terminates_on_shared_and_cyclic_containers() {
    ok(
        r#"
        let
          shared = { leaf = 1; };
          attrs = rec { self = attrs; inherit shared; again = shared; };
          list = let x = [ x shared shared ]; in x;
        in builtins.deepSeq attrs (builtins.deepSeq list true)
        "#,
        "true",
    );
}

#[test]
fn arithmetic_and_output() {
    ok("1 + 2 * 3", "7");
    ok("7 / 2", "3");
    ok("7.0 / 2", "3.5");
    ok("1.0 + 2", "3");
    ok("42.0", "42");
    ok("1.5e300", "1.5e+300");
    ok("builtins.sub 0 9223372036854775807", "-9223372036854775807");
    fails(
        "9223372036854775807 + 1",
        "integer overflow in adding 9223372036854775807 + 1",
    );
    fails("1 / 0", "division by zero");
    fails("1 + \"x\"", "cannot add a string to an integer");
}

#[test]
fn strings_paths_concat() {
    ok(r#""a" + "b""#, r#""ab""#);
    ok(r#"/foo + "x""#, "/foox");
    ok(r#"./x + /y"#, "/test/x/y");
    ok(r#""esc: \n \t \" \\ \${}""#, r#""esc: \n \t \" \\ \${}""#);
    ok(r#"builtins.toString [true false null 1 1.5]"#, r#""1   1 1.500000""#);
    ok(r#"toString { __toString = self: 5; }"#, r#""5""#);
}

/// `builtins.concatStringsSep` must coerce path elements with C++
/// `coerceToString`'s default `copyToStore = true`: a path (e.g. `./ldexpl.c`)
/// is copied to the store and rendered as its `/nix/store/...` path, not the
/// raw source path. This governs the NixOS minimal-bootstrap `mes-libc` builder
/// (`firstLibc = ... ++ [ ./ldexpl.c ]`); getting it wrong diverges the whole
/// cross-system Linux stdenv from the C++ reference.
#[test]
fn concat_strings_sep_copies_path_to_store() {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("jinx_cssep_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("ldexpl.c");
    std::fs::File::create(&file)
        .unwrap()
        .write_all(b"int ldexpl(void){return 0;}\n")
        .unwrap();
    let abs = file.to_string_lossy().into_owned();

    let out = eval(&format!(r#"builtins.concatStringsSep " " [ "a" {abs} ]"#))
        .expect("concatStringsSep with a path element");
    // The path is copied to the store: rendered as /nix/store/<hash>-ldexpl.c,
    // never the raw temp path.
    assert!(
        out.starts_with(r#""a /nix/store/"#) && out.ends_with(r#"-ldexpl.c""#),
        "expected store path, got {out}"
    );
    assert!(
        !out.contains(abs.as_str()),
        "raw source path leaked into result: {out}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Create a temp file with the given contents and return its absolute path as a
/// bare (unquoted) Nix path literal. Used by the coerceToString-flag tests to
/// feed a real path through builtins that copy it to the store.
fn temp_path(tag: &str, contents: &[u8]) -> (std::path::PathBuf, String) {
    use std::io::Write;
    let dir = std::env::temp_dir().join(format!("jinx_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("f.txt");
    std::fs::File::create(&file).unwrap().write_all(contents).unwrap();
    let abs = file.to_string_lossy().into_owned();
    (dir, abs)
}

/// `builtins.stringLength`, `builtins.substring`, `builtins.abort`,
/// `builtins.throw`, and the `unsafeDiscard*` builtins all inherit C++
/// `coerceToString`'s default `copyToStore = true`. A path argument must be
/// copied to the store and rendered as its `/nix/store/<hash>-f.txt` path, not
/// the raw source path. Oracle-verified (`nix-instantiate --eval`).
#[test]
fn string_builtins_copy_path_to_store() {
    let (dir, abs) = temp_path("strb", b"hello contents\n");
    let store_len = "/nix/store/0123456789abcdefghijklmnopqrstuv-f.txt".len(); // 48

    // stringLength counts the store path, not the raw temp path.
    let out = eval(&format!("builtins.stringLength {abs}")).expect("stringLength(path)");
    assert_eq!(out, store_len.to_string(), "stringLength should count store path");

    // substring over the whole string yields the store path.
    let out = eval(&format!("builtins.substring 0 200 {abs}")).expect("substring(path)");
    assert!(
        out.starts_with(r#""/nix/store/"#) && out.ends_with(r#"-f.txt""#) && !out.contains(abs.as_str()),
        "substring should render the store path, got {out}"
    );

    // unsafeDiscardStringContext coerces (copying to store) then drops context.
    let out = eval(&format!("builtins.unsafeDiscardStringContext {abs}"))
        .expect("unsafeDiscardStringContext(path)");
    assert!(
        out.starts_with(r#""/nix/store/"#) && out.ends_with(r#"-f.txt""#) && !out.contains(abs.as_str()),
        "unsafeDiscardStringContext should render the store path, got {out}"
    );

    // abort / throw embed the coerced store path in their error message.
    let e = eval(&format!("builtins.abort {abs}")).expect_err("abort(path)");
    assert!(
        e.contains("/nix/store/") && e.contains("-f.txt") && !e.contains(abs.as_str()),
        "abort message should contain the store path, got {e}"
    );
    let e = eval(&format!("builtins.throw {abs}")).expect_err("throw(path)");
    assert!(
        e.contains("/nix/store/") && e.contains("-f.txt") && !e.contains(abs.as_str()),
        "throw message should contain the store path, got {e}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn scoping_let_rec_with() {
    ok("let x = 1; y = x + 1; in x + y", "3");
    ok("rec { a = b + 1; b = 2; }.a", "3");
    ok("let x = 1; in with { x = 2; }; x", "1"); // lexical wins
    ok("with { x = 2; }; with { x = 3; }; x", "3"); // innermost with
    ok("let inherit (rec { a = 5; b = a; }) b; in b", "5");
    ok(
        "with rec { g = n: if n == 0 then 0 else g (n - 1); }; g 10",
        "0",
    );
    ok("with { x = { y = 7; }; }; with x; y", "7");
    fails("let x = x; in x", "infinite recursion encountered");
}

#[test]
fn rec_overrides() {
    ok(
        r#"(rec { a = b; b = 1; __overrides = { b = 2; }; }).a"#,
        "2",
    );
}

#[test]
fn functions_and_formals() {
    ok("(x: y: x + y) 1 2", "3");
    ok("({ a, b ? a + 1 }: a + b) { a = 1; }", "3");
    ok("({ a, b ? a + 1 }@args: args.a + b) { a = 1; }", "3");
    ok("({ ... }@args: args.z) { z = 9; }", "9");
    ok("{ __functor = self: x: self.n + x; n = 10; } 5", "15");
    fails(
        "({ a }: a) { a = 1; b = 2; }",
        "called with unexpected argument 'b'",
    );
    fails("({ a }: a) { }", "called without required argument 'a'");
}

#[test]
fn equality_semantics() {
    ok("let f = x: x; in f == f", "false"); // top-level function compare
    ok("let s = { f = x: x; }; in s == s", "true"); // shared cells deeper
    // Repeated instances from one static constructor share their key shape;
    // equality still compares every value.
    ok(
        "let mk = x: { a = x; b = x + 1; }; in mk 4 == mk 4",
        "true",
    );
    ok(
        "let mk = x: { a = x; b = x + 1; }; in mk 4 == mk 5",
        "false",
    );
    // Separate source sites may have equal names but distinct positions and
    // descriptors. Positions are not part of value equality.
    ok("{ a = 1; b = 2; } == { a = 1; b = 2; }", "true");
    ok("{ a = 1; b = 2; } == { a = 1; c = 2; }", "false");
    // Static-shape specialization remains behind derivation equality, which
    // compares only outPath and must not force unrelated fields.
    ok(
        r#"let mk = p: { type = "derivation"; outPath = p; ignored = throw "lazy"; }; in mk "/a" == mk "/a""#,
        "true",
    );
    ok(
        r#"let mk = p: { type = "derivation"; outPath = p; ignored = throw "lazy"; }; in mk "/a" == mk "/b""#,
        "false",
    );
    ok("1 == 1.0", "true");
    ok("[1 2] == [1 2.0]", "true");
    ok(r#"{ a = 1; } == { a = 1; b = 2; }"#, "false");
    // Equality against [] is specialized but must keep lazy list elements and
    // non-list cross-type behavior unchanged.
    ok("[ (throw \"unused\") ] == []", "false");
    ok("(x: x) == []", "false");
    ok("[] != builtins.tail [ 1 ]", "false");
}

#[test]
fn laziness_and_memoization() {
    // Errors are memoized: the same thrown error is rethrown on re-force.
    ok(
        r#"let foo = throw "nope"; in
           builtins.seq (builtins.tryEval foo).success
           (builtins.seq (builtins.tryEval foo).success "done")"#,
        r#""done""#,
    );
    // tryEval does not catch abort.
    fails(r#"(builtins.tryEval (abort "x")).success"#, "evaluation aborted");
    // Unused list elements stay unevaluated.
    ok(r#"builtins.length [ (throw "a") (throw "b") ]"#, "2");
    ok(r#"builtins.replaceStrings ["oo"] [(throw "no")] "xy""#, r#""xy""#);
}

#[test]
fn list_and_attr_builtins() {
    ok("builtins.sort builtins.lessThan [3 1 2]", "[ 1 2 3 ]");
    // Stable sort.
    ok(
        "map (x: x.k) (builtins.sort (a: b: a.o < b.o) [ {o=1;k=1;} {o=0;k=2;} {o=1;k=3;} {o=0;k=4;} ])",
        "[ 2 4 1 3 ]",
    );
    ok("builtins.attrNames { b = 1; a = 2; \"c c\" = 3; }", r#"[ "a" "b" "c c" ]"#);
    ok(
        "builtins.listToAttrs [ {name=\"a\"; value=1;} {name=\"a\"; value=2;} ]",
        "{ a = 1; }",
    );
    ok("builtins.foldl' (a: b: a + b) 0 [1 2 3 4]", "10");
    ok("builtins.length [ (throw \"lazy\") ]", "1");
    ok("__head [ 7 ]", "7");
    // Spelling alone is insufficient for specialization: lexical shadowing
    // and overapplication stay on generic call semantics.
    ok("let builtins = { head = _: 42; }; in builtins.head []", "42");
    ok("let __length = _: 9; in __length []", "9");
    // More tails than the bounded inline-view limit exercise both the shared
    // view and materializing fallback without changing list semantics.
    ok(
        "let drop = n: xs: if n == 0 then xs else drop (n - 1) (builtins.tail xs); in builtins.head (drop 40 (builtins.genList (x: x) 50))",
        "40",
    );
    ok(
        "builtins.genericClosure { startSet = [ {key = 0;} ]; operator = x: if x.key < 3 then [ {key = x.key + 1;} ] else []; }",
        "[ { key = 0; } { key = 1; } { key = 2; } { key = 3; } ]",
    );
    ok(
        "builtins.genericClosure { startSet = [ {key=2;} {key=1;} {key=2;} ]; operator = x: []; }",
        "[ { key = 2; } { key = 1; } ]",
    );
    ok(
        r#"builtins.genericClosure { startSet = [ {key="a";} {key="b";} {key="a";} ]; operator = x: []; }"#,
        r#"[ { key = "a"; } { key = "b"; } ]"#,
    );
    // Mixed numeric keys retain CompareValues equality through hash fallback.
    ok(
        "builtins.genericClosure { startSet = [ {key=1;} {key=1.0;} ]; operator = x: []; }",
        "[ { key = 1; } ]",
    );
    fails(
        r#"builtins.genericClosure { startSet = [ {key=1;} {key="x";} ]; operator = x: []; }"#,
        "cannot compare a string with an integer",
    );
    ok(
        "builtins.partition (x: x > 2) [1 3 2 4]",
        "{ right = [ 3 4 ]; wrong = [ 1 2 ]; }",
    );
}

#[test]
fn dynamic_attrs() {
    ok(r#"{ "${"a" + "b"}" = 1; }"#, "{ ab = 1; }");
    ok(r#"{ ${null} = 1; }"#, "{ }");
    fails(r#"{ "${null}" = 1; }"#, "cannot coerce null to a string");
    fails(
        r#"{ a = 1; "${"a"}" = 2; }"#,
        "dynamic attribute 'a' already defined",
    );
}

#[test]
fn select_and_or() {
    ok("{ a.b.c = 1; }.a.b.c", "1");
    ok("{ a = 1; }.b or 2", "2");
    ok("(1).b or 2", "2"); // non-attrs with `or` takes the default
    ok("{ a = { b = 1; }; } ? a.b", "true");
    ok("{ a = 1; } ? a.b", "false");
    fails("{ a = 1; }.abc", "attribute 'abc' missing");
}

#[test]
fn json_roundtrip() {
    ok("builtins.toJSON 42.0", r#""42.0""#);
    ok("builtins.toJSON 0.1", r#""0.1""#);
    ok("builtins.toJSON [1.5e300]", r#""[1.5e+300]""#);
    ok(
        r#"builtins.toJSON { b = [true null]; a = "x\ny"; }"#,
        r#""{\"a\":\"x\\ny\",\"b\":[true,null]}""#,
    );
    ok(r#"builtins.fromJSON "{\"a\": [1, 2.5, \"s\"]}""#, r#"{ a = [ 1 2.5 "s" ]; }"#);
}

#[test]
fn repeated_detection() {
    // The outer list element aliases x's cell (maybeThunk), so the cycle is
    // detected one level in — matches nix-instantiate.
    ok("let x = [ x ]; in [ x ]", "[ [ «repeated» ] ]");
    ok("[ [] [] {} {} ]", "[ [ ] [ ] { } { } ]");
}

#[test]
fn versions() {
    ok(r#"builtins.compareVersions "1.0" "2.3""#, "-1");
    ok(r#"builtins.compareVersions "2.1" "2.1.0.0""#, "-1");
    ok(r#"builtins.compareVersions "2.1pre1" "2.1""#, "-1");
    ok(r#"builtins.splitVersion "1.2a.3-4""#, r#"[ "1" "2" "a" "3" "4" ]"#);
    ok(
        r#"builtins.parseDrvName "nix-0.12pre13020""#,
        r#"{ name = "nix"; version = "0.12pre13020"; }"#,
    );
}

#[test]
fn gc_survives_heavy_allocation() {
    // Enough garbage to trigger collections even at the default threshold
    // is too slow for a unit test; instead verify a long allocation-heavy
    // computation stays correct (the conformance suite is additionally run
    // under JINX_GC_STRESS=1).
    ok(
        "builtins.length (builtins.filter (x: x / 2 * 2 == x) (builtins.genList (i: i) 10000))",
        "5000",
    );
    ok(
        "builtins.length (builtins.attrNames (builtins.foldl' (acc: n: acc // { \"k${toString n}\" = n; }) {} (builtins.genList (i: i) 300)))",
        "300",
    );
}

#[test]
fn fold_gen_fusion_preserves_laziness_and_sharing_edges() {
    // Direct and conservatively single-use let-bound producer forms.
    ok(
        "builtins.foldl' (a: x: a + x) 0 (builtins.genList (i: i + 1) 5)",
        "15",
    );
    ok(
        "let xs = builtins.genList (i: i + 1) 5; in builtins.foldl' (a: x: a + x) 0 xs",
        "15",
    );

    // Generated elements stay lazy when the consumer ignores them.
    ok(
        "builtins.foldl' (a: _: a + 1) 0 (builtins.genList (_: abort \"unused\") 5)",
        "5",
    );
    // If the accumulator retains an element, releasing the iteration roots
    // must not collect it.
    ok(
        "builtins.foldl' (a: x: [x] ++ a) [] (builtins.genList (i: i) 4)",
        "[ 3 2 1 0 ]",
    );

    // A shared let producer is deliberately rejected by the use-count proof.
    ok(
        "let xs = builtins.genList (i: i) 4; in [ (builtins.foldl' (a: x: a + x) 0 xs) (builtins.length xs) ]",
        "[ 6 4 ]",
    );
    // Empty genList still validates/forces its function before foldl' forces z.
    fails(
        "builtins.foldl' (a: _: a) (abort \"late\") (builtins.genList 1 0)",
        "expected a function",
    );
}

#[test]
fn fold_gen_fusion_rejects_shadowing_and_partial_calls() {
    ok(
        "let builtins = { foldl' = f: z: xs: 99; genList = f: n: []; }; in builtins.foldl' (a: b: a) 0 (builtins.genList (x: x) 3)",
        "99",
    );
    ok(
        "let __foldl' = f: z: xs: 88; in __foldl' (a: b: a) 0 (builtins.genList (x: x) 3)",
        "88",
    );
    ok(
        "let p = builtins.foldl' (a: x: a + x) 0; in p (builtins.genList (x: x) 4)",
        "6",
    );
}

#[test]
fn call_depth_limit() {
    fails(
        "let f = n: f (n + 1); in f 0",
        "stack overflow; max-call-depth exceeded",
    );
}
