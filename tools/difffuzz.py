#!/usr/bin/env python3
"""Differential fuzzer: jinx vs the C++ Nix oracle.

Generates random *valid* pure Nix expressions, evaluates each under both
engines (--eval --strict), and reports divergences. To be robust against the
oracle being an older Nix (2.33.3) than jinx targets (2.36pre), we compare
SEMANTICS not message text:
  - both succeed with the same value  -> match
  - both error (any message)          -> match (error-class parity is enough)
  - one succeeds, other errors         -> DIVERGENCE
  - both succeed, different values      -> DIVERGENCE
  - jinx crashes/times out, nix doesn't -> DIVERGENCE (crash)

Each expr is wrapped `builtins.tryEval (builtins.deepSeq E E)` and K of them are
put in one list so a single process launch covers many exprs; a divergent batch
is bisected down to the single culprit.
"""
import random, subprocess, sys, time, os

_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
JINX = os.environ.get("JINX", os.path.join(_ROOT, "target/release/jinx"))
ORACLE = os.environ.get("ORACLE", os.path.join(_ROOT, ".oracle/bin/nix-instantiate"))
if not os.path.exists(ORACLE):
    ORACLE = "nix-instantiate"

INTS = ["0","1","2","3","(-1)","(-5)","7","255","1000000","(-2147483648)","9223372036854775807","(-9223372036854775808)","4611686018427387904","(-9223372036854775807)"]
FLOATS = ["0.0","1.5","(-2.5)","3.14159","1.0e10","1.0e-5","0.1","100.0","(-0.0)","1.0e308","1.7976931348623157e308","5.0e-324","3.0e300","(-3.0e300)","0.5","2.220446049250313e-16"]
STRS = ['""','"a"','"abc"','"hello world"','"\\n"','"\\t"','"foo/bar"','"a:b"','"café"','"1.5"','"true"','"[1 2]"','"%s"','"\\\\"']
BOOLS = ["true","false"]
ATOMS = INTS+FLOATS+STRS+BOOLS+["null"]
WEIRD = ["{ __toString = self: \"s\"; }","{ __functor = self: x: x; }","{ outPath = \"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x\"; }","{ a.b.c = 1; }","[[[1]]]","{ __toString = self: self; }","(x: x)","builtins.null or 0"]
LAMBDAS = ["(x: x)","(x: x + 1)","(x: x * 2)","(x: builtins.toString x)","(a: b: a + b)",
           "(x: [x])","(x: {inherit x;})","(x: x == 0)","(x: -x)","(x: builtins.typeOf x)",
           "(k: v: v)","(k: v: {name=k; value=v;})"]

def gen_list(d):
    n = random.randint(0,4)
    return "[ " + " ".join(gen(d-1) for _ in range(n)) + " ]"
def gen_attrs(d):
    n = random.randint(0,3)
    keys = random.sample(["a","b","c","x","y","name","value","_1"], n)
    return "{ " + " ".join(f"{k} = {gen(d-1)};" for k in keys) + " }"

# (template, min_depth) — templates splice sub-exprs via {} with gen()
BIN_NUM = ["({0} + {1})","({0} - {1})","({0} * {1})","(builtins.bitAnd {0} {1})","(builtins.bitOr {0} {1})","(builtins.bitXor {0} {1})","(builtins.add {0} {1})","(builtins.mul {0} {1})","(builtins.sub {0} {1})"]
BIN_CMP = ["({0} < {1})","({0} > {1})","({0} <= {1})","({0} >= {1})","({0} == {1})","({0} != {1})"]
BIN_BOOL= ["({0} && {1})","({0} || {1})","(!{0})"]
STR_OPS = ['(builtins.substring {N0} {N1} {0})','(builtins.stringLength {0})','(builtins.toString {0})',
           '(builtins.concatStringsSep {0} {L})','(builtins.replaceStrings {L} {L} {0})',
           '(builtins.split {0} {1})','(builtins.match {0} {1})','(builtins.compareVersions {0} {1})',
           '(builtins.splitVersion {0})','({0} + {1})']
LIST_OPS= ['(builtins.length {L})','(builtins.elemAt {L} {N0})','(builtins.head {L})','(builtins.tail {L})',
           '(builtins.elem {0} {L})','({L} ++ {L})','(builtins.concatLists {L})','(map {F} {L})',
           '(builtins.filter {F} {L})','(builtins.genList {F} {N0})','(builtins.sort {F} {L})',
           '(builtins.foldl\' {F} {0} {L})','(builtins.all {F} {L})','(builtins.any {F} {L})',
           '(builtins.concatMap {F} {L})','(builtins.partition {F} {L})']
ATTR_OPS= ['(builtins.attrNames {A})','(builtins.attrValues {A})','(builtins.hasAttr {0} {A})',
           '({A} // {A})','(builtins.removeAttrs {A} {L})','(builtins.intersectAttrs {A} {A})',
           '(builtins.mapAttrs {F} {A})','(builtins.listToAttrs {L})','(builtins.catAttrs {0} {L})',
           '(builtins.functionArgs {F})','({A}.a or {0})']
TYPE_OPS= ['(builtins.typeOf {0})','(builtins.isInt {0})','(builtins.isString {0})','(builtins.isList {0})',
           '(builtins.isAttrs {0})','(builtins.isBool {0})','(builtins.isFunction {0})','(builtins.isNull {0})','(builtins.isFloat {0})']
MISC    = ['(builtins.floor {0})','(builtins.ceil {0})','(builtins.seq {0} {1})','(builtins.deepSeq {0} {1})',
           '(builtins.toJSON {0})','(builtins.fromJSON (builtins.toJSON {0}))','(builtins.tryEval {0})',
           '(if {0} then {1} else {2})','(builtins.throw {0})','(builtins.abort {0})',
           '(builtins.hashString "sha256" {0})','(builtins.stringLength (builtins.toJSON {0}))',
           '(builtins.fromJSON {0})','(builtins.toString ({0} + {1}))','(builtins.elemAt (builtins.splitVersion {0}) {N0})']

ALLGROUPS = [BIN_NUM,BIN_CMP,BIN_BOOL,STR_OPS,LIST_OPS,ATTR_OPS,TYPE_OPS,MISC]

def fill(t,d):
    return t.replace("{0}",gen(d-1)).replace("{1}",gen(d-1)).replace("{2}",gen(d-1)) \
            .replace("{N0}",random.choice(INTS)).replace("{N1}",random.choice(INTS)) \
            .replace("{L}",gen_list(d-1)).replace("{A}",gen_attrs(d-1)).replace("{F}",random.choice(LAMBDAS))

def gen(d):
    if d<=0 or random.random()<0.30:
        r=random.random()
        if r<0.60: return random.choice(ATOMS)
        if r<0.72: return random.choice(WEIRD)
        if r<0.86: return gen_list(1)
        return gen_attrs(1)
    grp=random.choice(ALLGROUPS)
    return fill(random.choice(grp),d)

def wrap(e):
    # tryEval+deepSeq forces the value and catches recoverable errors so a
    # single expr can't (usually) abort the whole batch.
    return "(let e = ("+e+"); in builtins.tryEval (builtins.deepSeq e e))"

def run(engine, src, timeout):
    try:
        p=subprocess.run([engine,"--eval","--strict","-E",src],capture_output=True,text=True,timeout=timeout)
        out="\n".join(l for l in p.stdout.splitlines() if not l.startswith("warning:"))
        err="\n".join(l for l in p.stderr.splitlines() if not l.startswith("warning:"))
        return (p.returncode,out,err)
    except subprocess.TimeoutExpired:
        return ("TIMEOUT","","")

def norm(rc,out,err):
    # semantic bucket: success+value, or generic "ERR" (any error class)
    if rc==0: return ("ok",out.strip())
    if rc=="TIMEOUT": return ("timeout","")
    if rc<0 or rc>=128 or "stack overflow" in err or "SIGABRT" in err: return ("crash",err[-200:])
    return ("err","")   # both engines erroring is a match regardless of message

def batch_expr(exprs):
    return "[ " + " ".join(wrap(e) for e in exprs) + " ]"

def compare_one(e):
    src=batch_expr([e])
    j=norm(*run(JINX,src,10)); n=norm(*run(ORACLE,src,10))
    return j,n

def main():
    budget=float(sys.argv[1]) if len(sys.argv)>1 else 300.0
    seed=int(sys.argv[2]) if len(sys.argv)>2 else 12345
    random.seed(seed)
    t0=time.time(); nexpr=0; nbatch=0; divs=[]
    K=25
    while time.time()-t0 < budget:
        exprs=[gen(random.randint(2,4)) for _ in range(K)]
        src=batch_expr(exprs)
        j=run(JINX,src,20); n=run(ORACLE,src,20)
        nbatch+=1; nexpr+=K
        jb=norm(*j); nb=norm(*n)
        if jb!=nb:
            # bisect: find culprit expr(s)
            for e in exprs:
                je,ne=compare_one(e)
                if je!=ne:
                    divs.append((e,je,ne))
                    print(f"DIVERGENCE: {e!r}\n  jinx={je}\n  nix ={ne}",flush=True)
        if nbatch%20==0:
            print(f"[{time.time()-t0:.0f}s] {nexpr} exprs, {nbatch} batches, {len(divs)} divergences",file=sys.stderr,flush=True)
    print(f"\n=== DONE: {nexpr} exprs in {time.time()-t0:.0f}s, {len(divs)} divergences ===")
    for e,je,ne in divs:
        print(f"---\n{e}\n  jinx={je}\n  nix ={ne}")

if __name__=="__main__":
    main()
