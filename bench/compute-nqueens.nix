# Combinatorial search kernel. Count solutions to the 10-queens problem while
# exercising recursive functions, closures, list traversal, and integer tests.
let
  n = 10;
  range = builtins.genList (i: i) n;
  safe = row: cols:
    let
      check = distance: rest:
        if rest == [] then true
        else
          let previous = builtins.head rest;
          in previous != row
             && previous != row - distance
             && previous != row + distance
             && check (distance + 1) (builtins.tail rest);
    in check 1 cols;
  count = cols:
    if builtins.length cols == n then 1
    else builtins.foldl'
      (total: row: total + (if safe row cols then count ([ row ] ++ cols) else 0))
      0
      range;
in
count []
