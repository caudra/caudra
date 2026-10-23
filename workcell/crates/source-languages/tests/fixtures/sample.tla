---- MODULE sample ----
EXTENDS Naturals, Sequences

CONSTANTS Runs, Limit
CONSTANT Default(_)
VARIABLES pc, count
VARIABLE log

ASSUME LimitPositive == Limit > 0

vars == <<pc, count, log>>

Counter == INSTANCE Counters WITH n <- count
INSTANCE TLC

LOCAL Helper == 1

Wrapped == Counter!Inc(1)

Init ==
  /\ pc = [r \in Runs |-> "idle"]
  /\ count = 0
  /\ log = <<>>

double[n \in Nat] == 2 * n

Bump(r, by) ==
  LET next == count + by
      Clamp(x) == IF x > Limit THEN Limit ELSE x
  IN /\ count' = Clamp(next)
     /\ UNCHANGED <<pc, log>>

Step(r) == Bump(r, double[1]) /\ log' = Append(log, r)

a \oplus b == a + b

Next == \E r \in Runs : Step(r)

Spec == Init /\ [][Next]_vars

THEOREM Safety == Spec => [](count <= Limit)
LEMMA Trivial == TRUE

(* --algorithm Transfer
variables total = 0;
define
  Busy == total > 0
end define;

macro Reset() begin
  total := 0;
end macro;

procedure Add(n) begin
  AddStep: total := total + n;
  return;
end procedure;

process Worker \in Runs
begin
  Work: call Add(1);
  Done: Reset();
end process;
end algorithm; *)
====
