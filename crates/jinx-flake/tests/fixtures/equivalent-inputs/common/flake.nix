{
  description = "common equivalent input";

  outputs = { self }: {
    # Large enough that evaluating the duplicate graph is measurable, while
    # the result stays tiny and deterministic for the trace-counting test.
    value = builtins.trace "equivalent-input-evaluated" (
      builtins.deepSeq (builtins.genList (n: n * n) 100000) "x"
    );
  };
}
