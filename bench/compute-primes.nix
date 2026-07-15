# Integer/list kernel. Trial division is intentionally expressed in Nix so the
# result measures evaluator work rather than an external helper or I/O.
let
  limit = 50000;
  candidates = builtins.genList (i: i + 2) (limit - 1);
  isPrime = value:
    let
      trial = divisor:
        if divisor * divisor > value then true
        else if builtins.div value divisor * divisor == value then false
        else trial (divisor + 1);
    in
    trial 2;
in
builtins.length (builtins.filter isPrime candidates)
