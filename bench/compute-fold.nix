# Strict numerical reduction. This keeps the final result scalar while doing
# repeated closure calls, forcing, multiplication, and addition over one million
# inputs.
let
  values = builtins.genList (i: i) 1000000;
in
builtins.foldl' (sum: value: sum + value * value + 3 * value + 7) 0 values
