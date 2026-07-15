let
  closure = builtins.genericClosure {
    startSet = [ { key = 0; } ];
    operator = x:
      if x.key < 20000
      then [ { key = x.key + 1; } ]
      else [ ];
  };
in builtins.length closure
