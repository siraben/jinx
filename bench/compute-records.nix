let
  count = 200000;
  mkRecord = i: {
    alpha = i;
    beta = i + 1;
    gamma = i * 3;
    delta = i - 7;
    even = builtins.bitAnd i 1 == 0;
    label = if i < 100000 then "low" else "high";
  };
  step = acc: i:
    let
      left = mkRecord i;
      right = mkRecord i;
    in
      if left == right
      then acc + left.gamma
      else throw "equal records compared unequal";
in
  builtins.foldl' step 0 (builtins.genList (i: i) count)
