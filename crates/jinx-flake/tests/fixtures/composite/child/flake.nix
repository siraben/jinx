{
  description = "child flake";
  outputs = { self }: {
    value = "child-42";
  };
}
