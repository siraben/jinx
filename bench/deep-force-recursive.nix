let
  # Wide acyclic containers with many scalar leaves exercise the avoided
  # leaf-set inserts. Keep the arithmetic lazy until deepSeq walks the graph.
  wide = builtins.genList (n: {
    a = n;
    b = n + 1;
    c = [ n (n + 2) (n + 3) ];
  }) 50000;

  # Repeated references prove shared recursive containers are visited once.
  sharedNode = {
    values = builtins.genList (n: n * n) 1000;
    label = "shared";
  };
  shared = builtins.genList (_: sharedNode) 10000;

  # True cycles must still terminate. Both supported recursive container
  # shapes are present so the seen set remains a correctness requirement.
  cyclicAttrs = rec {
    self = cyclicAttrs;
    leaf = 42;
  };
  cyclicList = let x = [ x ]; in x;
in
builtins.deepSeq wide (
  builtins.deepSeq shared (
    builtins.deepSeq cyclicAttrs (
      builtins.deepSeq cyclicList true
    )
  )
)
