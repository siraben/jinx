# Call-heavy integer kernel. The deliberately naive recurrence stresses
# function calls, conditional dispatch, forcing, and small-integer arithmetic.
let
  fib = n: if n < 2 then n else fib (n - 1) + fib (n - 2);
in
fib 32
