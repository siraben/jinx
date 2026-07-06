# Compute-shaped microbenchmark: naive fibonacci (call-heavy, int arithmetic).
let fib = n: if n < 2 then n else fib (n - 1) + fib (n - 2);
in fib 27
