# Allocation/list-shaped microbenchmark: genList / foldl' / sort / attrsets.
let
  n = 100000;
  xs = builtins.genList (i: i * 7919) n;
  sum = builtins.foldl' (a: b: a + b) 0 xs;
  sorted = builtins.sort (a: b: a < b) (builtins.genList (i: (i * 2654435761) - (i * i)) 2000);
  attrs = builtins.listToAttrs (builtins.genList (i: { name = "k${toString i}"; value = i; }) 5000);
in { inherit sum; first = builtins.head sorted; count = builtins.length (builtins.attrNames attrs); }
