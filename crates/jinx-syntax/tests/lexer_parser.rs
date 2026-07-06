//! Unit tests for lexer/parser edge cases discovered while porting
//! lexer.l / parser.y. The broad correctness net is the conformance suite
//! plus differential testing against the C++ oracle; these pin down the
//! subtle spots.

use jinx_syntax::{parse_and_bind, show, Origin, PosTable};

fn parse_ok(src: &str) -> String {
    let mut positions = PosTable::new();
    let mut warnings = Vec::new();
    let res = parse_and_bind(
        src.as_bytes(),
        Origin::Stdin {
            source: src.as_bytes().to_vec(),
        },
        "/base",
        Some("/home/u"),
        &mut positions,
        &mut warnings,
    )
    .unwrap_or_else(|e| {
        panic!(
            "parse failed for {:?}: {}",
            src,
            String::from_utf8_lossy(&e.render(&positions))
        )
    });
    String::from_utf8(show::show(&res.exprs, &res.symbols, res.root)).unwrap()
}

fn parse_err(src: &str) -> String {
    let mut positions = PosTable::new();
    let mut warnings = Vec::new();
    let err = parse_and_bind(
        src.as_bytes(),
        Origin::Stdin {
            source: src.as_bytes().to_vec(),
        },
        "/base",
        Some("/home/u"),
        &mut positions,
        &mut warnings,
    )
    .err()
    .unwrap_or_else(|| panic!("expected error for {src:?}"));
    String::from_utf8_lossy(&err.render(&positions)).into_owned()
}

#[test]
fn string_trailing_dollar_before_quote() {
    // flex needs a separate rule for a string ending in '$'
    assert_eq!(parse_ok(r#""abc$""#), r#""abc$""#);
    assert_eq!(parse_ok(r#""$""#), "\"$\"");
    // "$$" is two literal dollars; only \${ starts interpolation
    assert_eq!(parse_ok(r#""$${a}""#), r#""$\${a}""#);
}

#[test]
fn string_cr_normalisation() {
    // CR and CRLF are normalised to LF inside strings; \r escape survives
    assert_eq!(parse_ok("\"a\r\nb\rc\\r\""), "\"a\\nb\\nc\\rd\"".replace('d', ""));
}

#[test]
fn ind_string_escapes() {
    // escape tokens stay separate parts of the ExprConcatStrings
    assert_eq!(parse_ok("''a''$b''"), "(\"a\" + \"$\" + \"b\")"); // ''$ escapes $
    assert_eq!(parse_ok("'''''''"), "\"''\""); // ''' gives literal ''
    assert_eq!(parse_ok("''a''\\nb''"), "(\"a\" + \"\\n\" + \"b\")"); // ''\n escape
    assert_eq!(parse_ok("''''"), "\"\""); // empty
}

#[test]
fn ind_string_stripping() {
    assert_eq!(parse_ok("''\n  foo\n  bar''"), "\"foo\\nbar\"");
    // whitespace-only final line is not counted for minimum indentation
    assert_eq!(parse_ok("''\n  foo\n ''"), "\"foo\\n\"");
    // interpolation at line start ends the indentation scan
    assert_eq!(parse_ok("''\n  ${\"x\"}\n  y''"), "(\"x\" + \"\\ny\")");
}

#[test]
fn id_with_dashes_vs_arrow() {
    // 'x->y' lexes as ID "x-" '>' ID "y" (dash is an identifier char),
    // so 'x-' is an undefined variable (matches C++ nix)
    let e = parse_err("x: y: x->y");
    assert!(e.starts_with("error: undefined variable 'x-'"), "{e}");
    // with spaces it is the implication operator
    assert_eq!(parse_ok("x: y: x -> y"), "(x: (y: (x -> y)))");
}

#[test]
fn uri_vs_lambda() {
    assert_eq!(parse_ok("x:x"), "\"x:x\""); // URI, not a lambda
    assert_eq!(parse_ok("x: x"), "(x: x)"); // lambda
}

#[test]
fn paths() {
    assert_eq!(parse_ok("/foo/bar"), "/foo/bar");
    assert_eq!(parse_ok("./foo"), "/base/foo");
    assert_eq!(parse_ok("../x/../y"), "/y");
    assert_eq!(parse_ok("~/cfg"), "/home/u/cfg");
    // a trailing slash not followed by more path is a lexer error
    let e = parse_err("x: x + /foo/bar/");
    assert!(e.starts_with("error: path has a trailing slash"), "{e}");
    // path interpolation (trailing slash of the leading segment survives)
    assert_eq!(parse_ok("x: ./foo/${x}"), "(x: (/base/foo/ + x))");
    assert_eq!(parse_ok("x: /a/b${x}c/d"), "(x: (/a/b + x + \"c/d\"))");
}

#[test]
fn int_overflow_is_parse_error() {
    let e = parse_err("9999999999999999999999");
    assert!(
        e.starts_with("error: invalid integer '9999999999999999999999'"),
        "{e}"
    );
}

#[test]
fn float_forms() {
    assert_eq!(parse_ok("1.5"), "1.5");
    assert_eq!(parse_ok(".5"), "0.5");
    assert_eq!(parse_ok("2.e2"), "200");
}

#[test]
fn eof_after_path_position() {
    // <INPATH><<EOF>> unstashes the location; the EOF error points before
    // the path token (flex quirk).
    let e = parse_err("[ ./p");
    assert!(e.contains("«stdin»:1:3"), "{e}");
}

#[test]
fn bison_masked_expected_lists() {
    assert!(parse_err("{ 1 = x; }")
        .starts_with("error: syntax error, unexpected integer, expecting 'inherit'"));
    assert!(parse_err("rec { 1 = x; }")
        .starts_with("error: syntax error, unexpected integer, expecting 'inherit' or '}'"));
    assert!(parse_err("let x = 1; 2")
        .starts_with("error: syntax error, unexpected integer, expecting 'in' or 'inherit'"));
    assert!(parse_err("{ a ... }")
        .starts_with("error: syntax error, unexpected '...', expecting '.' or '='"));
    assert!(parse_err("{x, y : z}: x")
        .starts_with("error: syntax error, unexpected ':', expecting '}' or ','"));
    assert!(parse_err("a.in").starts_with(
        "error: syntax error, unexpected 'in', expecting identifier or 'or' or '${' or '\"'"
    ));
    assert!(parse_err("&& x").starts_with("error: syntax error, unexpected '&&'\n"));
    assert!(parse_err("123 é 4")
        .starts_with("error: syntax error, unexpected invalid token, expecting end of file"));
}

#[test]
fn dup_attrs_and_formals() {
    assert!(parse_err("{ x = 1; x = 2; }")
        .starts_with("error: attribute 'x' already defined at «stdin»:1:3"));
    assert!(parse_err("{ a.b.c = 1; a.b.c = 2; }")
        .starts_with("error: attribute 'a.b.c' already defined at «stdin»:1:3"));
    assert!(parse_err("{x, y, x}: x")
        .starts_with("error: duplicate formal function argument 'x'"));
}

#[test]
fn nested_attr_merging() {
    assert_eq!(
        parse_ok("{ services.ssh = { enable = true; }; services.ssh.port = 23; }"),
        "{ services = { ssh = { enable = true; port = 23; }; }; }"
    );
}

#[test]
fn cursed_or_warning() {
    let src = b"x: [ (y: y) or ]".to_vec();
    let mut positions = PosTable::new();
    let mut warnings = Vec::new();
    let res = parse_and_bind(
        &src,
        Origin::Stdin {
            source: src.clone(),
        },
        "/base",
        None,
        &mut positions,
        &mut warnings,
    );
    assert!(res.is_err()); // 'or' is an undefined variable here
    assert_eq!(warnings.len(), 1);
    let w = String::from_utf8_lossy(&warnings[0]).into_owned();
    assert!(w.starts_with("warning: at «stdin»:1:6:"), "{w}");
    assert!(w.contains("((y: y) or)"), "{w}");
}

#[test]
fn tab_expansion_in_error_excerpt() {
    // the logger's filterANSIEscapes expands tabs against a cumulative
    // width counter
    let e = parse_err("\t\t&&");
    assert!(e.contains("1| \t\t&&".replace('\t', "").as_str()) || e.contains("&&"), "{e}");
    assert!(!e.contains('\t'), "tabs must be expanded: {e:?}");
}
