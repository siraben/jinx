{
  description = "composite flake";
  inputs.child.url = "path:./child";
  outputs = { self, child }: {
    combined = "parent+" + child.value;
  };
}
