# Comparison-heavy stable-sort kernel. A deterministic integer mix avoids an
# external random source while producing enough disorder for O(n log n)
# comparator work.
let
  size = 100000;
  sorted = builtins.sort (left: right: left < right)
    (builtins.genList (i: builtins.bitXor (i * 7919) (i * 104729 + 17)) size);
in
builtins.foldl' (sum: value: sum + value) 0 sorted
