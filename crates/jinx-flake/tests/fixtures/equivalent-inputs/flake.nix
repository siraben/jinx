{
  description = "equivalent flake input benchmark";

  outputs = { self, a, b, followed }: {
    combined = a.value + b.value + followed.value;
  };
}
