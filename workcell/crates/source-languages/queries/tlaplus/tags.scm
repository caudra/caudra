; Workcell TLA+ tags — authored against tree-sitter-tlaplus 1.5.0 (`tree_sitter_tlaplus::LANGUAGE`),
; used for `.tla`. Node kinds and fields verified with `examples/dump` against
; tests/fixtures/sample.tla and the grammar's node-types.json. Upstream ships only highlights and
; locals queries, so every pattern here is original.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────────
;   MODULE, M == INSTANCE X,     → @definition.module.
;   --algorithm
;   Op == e, Op(a) == e,         → @definition.function, at module scope, under LOCAL, and in a
;   f[x \in S] == e                PlusCal `define` block. A LET definition is local to its
;                                  expression and is not a symbol: the patterns are anchored on
;                                  their parent, so nothing inside a `let_in` matches.
;   CONSTANT(S), VARIABLE(S),    → @definition.constant. A variable is module-level state, the
;   THEOREM/LEMMA/ASSUME names     closest kind to a static; a named theorem or assumption is a
;                                  fact that `BY` cites.
;   PlusCal procedure            → @definition.function; macro → @definition.macro;
;                                  process → @definition.class.
;   a name in expression         → @reference.call. TLA+ spells a zero-arity operator application
;   position, Op(args), M!Op       and a variable read the same way, so `Next == Edit \/ Note` calls
;                                  `Edit` exactly as `Start(r)` calls `Start`, and an action that
;                                  reads or primes `pc` is an edge into `pc`. `(_expr/identifier_ref)`
;                                  matches only names in expression position, never the module
;                                  names of EXTENDS and INSTANCE.
;   EXTENDS M, INSTANCE M        → @reference.import.
;   PlusCal call / macro call    → @reference.call.
;   DISCLOSED LIMITATION: a user-defined infix, prefix, or postfix operator (`a \oplus b == …`) is
;   not a symbol, because its applications are indistinguishable from the standard operators'.
;   Labels are not symbols either: the PlusCal translation defines an operator per label, and a
;   label symbol would duplicate it.

; ---- modules ----
(module
  name: (identifier) @name) @definition.module

(module
  (module_definition
    name: (identifier) @name) @definition.module)

(local_definition
  (module_definition
    name: (identifier) @name) @definition.module)

(pcal_algorithm
  name: (identifier) @name) @definition.module

; ---- operators and functions ----
(module
  (operator_definition
    name: (identifier) @name) @definition.function)

(module
  (function_definition
    name: (identifier) @name) @definition.function)

(source_file
  (operator_definition
    name: (identifier) @name) @definition.function)

(source_file
  (function_definition
    name: (identifier) @name) @definition.function)

(local_definition
  (operator_definition
    name: (identifier) @name) @definition.function)

(local_definition
  (function_definition
    name: (identifier) @name) @definition.function)

(pcal_definitions
  (operator_definition
    name: (identifier) @name) @definition.function)

(pcal_definitions
  (function_definition
    name: (identifier) @name) @definition.function)

; ---- declarations and facts ----
(constant_declaration
  (identifier) @name @definition.constant)

(constant_declaration
  (operator_declaration
    name: (identifier) @name) @definition.constant)

(variable_declaration
  (identifier) @name @definition.constant)

(theorem
  name: (identifier) @name) @definition.constant

(assumption
  name: (identifier) @name) @definition.constant

; ---- PlusCal ----
(pcal_procedure
  (pcal_proc_decl
    name: (identifier) @name)) @definition.function

(pcal_macro
  (pcal_macro_decl
    name: (identifier) @name)) @definition.macro

(pcal_process
  name: (identifier) @name) @definition.class

(pcal_proc_call
  name: (identifier) @name) @reference.call

(pcal_macro_call
  name: (identifier) @name) @reference.call

; ---- references ----
(_expr/identifier_ref) @name @reference.call

(bound_op
  name: (identifier_ref) @name) @reference.call

(prefixed_op
  prefix: (subexpr_prefix
    (subexpr_component
      (identifier_ref) @qualifier))
  op: (identifier_ref) @name) @reference.call

(extends
  (identifier_ref) @name @reference.import)

(instance
  . (identifier_ref) @name @reference.import)
