; Workcell Gleam tags — authored against tree-sitter-gleam 1.0.0 (node kinds and field names
; verified with `examples/dump` against tests/fixtures/sample.gleam, cross-checked in the grammar's
; node-types.json). Upstream ships no tags.scm, so every pattern here is original.
;
; Deliberate limitations, all soundness rather than effort:
;   * A bare `use` callback, `todo`, `panic` and `echo` are keywords, not calls, so they mint no edge.
;   * `remote_constructor_name` (`shapes.Circle(1.0)`) is left uncaptured: the node fuses the module
;     and constructor segments and the grammar exposes no field to split them, so a @name minted from
;     it would either carry the module prefix or require guessing at token layout.
;   * A pipe whose right operand is an arbitrary expression (`x |> { fn(y) { y } }`) is not a call
;     pattern here; only the identifier and field-access spellings, which are the applied forms.

; ---- definitions ----

; Top-level and external functions. The capture sits on `function`, which owns `body:`; the
; `anonymous_function` node is a separate kind and is intentionally left anonymous.
(function
    name: (identifier) @name) @definition.function

(external_function
    name: (identifier) @name) @definition.function

; `const pi = ...` is a constant by construction — the keyword is the evidence, so no naming
; convention gate. The `name:` field keeps a constant whose value is a bare identifier
; (`const alias = pi`) from also matching its own right-hand side.
(constant
    name: (identifier) @name) @definition.constant

; Custom types and aliases. `type_definition` owns the constructor block, `type_alias` the aliased
; type; neither exposes a `name:` field on this grammar, so the name comes from the `type_name`
; child.
(type_definition
    (type_name
        (type_identifier) @name)) @definition.class

(type_alias
    (type_name
        (type_identifier) @name)) @definition.class

; Data constructors are the variants of a custom type and are first-class functions in Gleam, so
; they are definitions in their own right (same bucket as an enum case elsewhere). The capture sits
; on `data_constructor`, never on the `data_constructors` list wrapper, whose span is the whole
; block and would make every variant inherit the enclosing type's extent.
(data_constructor
    name: (constructor_name) @name) @definition.constant

; Labelled constructor arguments are the record's fields: `point.x` reads exactly this label.
(data_constructor_argument
    label: (label) @name) @definition.field

; ---- references ----

(import
    module: (module) @name) @reference.import

(unqualified_import
    name: (identifier) @name) @reference.import

; Plain application: `area(shape)`.
(function_call
    function: (identifier) @name) @reference.call

; The dominant Gleam call form is qualified: `io.println(...)`, `float.to_string(...)`. The
; alternation on `record:` binds @qualifier for the single-segment spelling that resolution can use
; and still matches the nested spelling (`a.b.c()`), which has no single qualifier segment to name.
(function_call
    function: (field_access
        record: [
            (identifier) @qualifier
            (field_access)
        ]
        field: (label) @name)) @reference.call

; Constructing a record applies the constructor function, so it is an edge into the
; `data_constructor` definition captured above.
(record
    name: (constructor_name) @name) @reference.call

; `x |> float.sum` and `x |> handler` apply the right operand with no argument list of their own, so
; the `function_call` patterns above never see them. Listing the `|>` token first is what pins the
; captured operand to the right-hand side: query siblings must match in order, so a left operand
; (which precedes the token) cannot satisfy these patterns.
(binary_expression
    "|>"
    (field_access
        record: [
            (identifier) @qualifier
            (field_access)
        ]
        field: (label) @name) @reference.call)

(binary_expression
    "|>"
    (identifier) @name @reference.call)
