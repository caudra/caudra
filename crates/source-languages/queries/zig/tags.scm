; Workcell Zig tags — authored against tree-sitter-zig 1.1.2 (node kinds and field names verified
; with `examples/dump` against tests/fixtures/sample.zig, cross-checked in the grammar's
; node-types.json). Upstream ships no tags.scm, so every pattern here is original.
;
; The shape that drives this file: Zig has no `struct`/`enum` *statement*. A type is a
; `variable_declaration` whose initializer happens to be a container, so the type/constant split is
; made by the initializer's node kind, and `variable_declaration` carries no `name:` field — the
; leading anchor is what pins @name to the bound identifier rather than to a right-hand side that is
; also a bare identifier (`const Allocator = Alloc;`).
;
; Deliberate limitations, all soundness rather than effort:
;   * Block-scope `const`/`var` are locals, not symbols, so the constant patterns enumerate the
;     file and container scopes instead of matching `variable_declaration` anywhere.
;   * A call whose receiver is itself a call or an index (`build().step()`, `list[0].run()`) mints
;     no edge: `object:` would have to be an open wildcard, which overlaps the identifier branch and
;     would double-count every ordinary `a.b()`.
;   * Builtins other than `@import` mint nothing. They are compiler intrinsics, not definitions in
;     any file, so an edge into them would point nowhere.

; ---- definitions ----

; Container types. The initializer alternation is the whole discriminator: these node kinds appear
; nowhere else in a declaration, so no naming convention is consulted. This also catches the
; generic-factory form (`fn List(comptime T: type) type { return struct { … }; }` binds no name, but
; `const Inner = struct { … };` inside one does).
(variable_declaration
    .
    (identifier) @name
    [
        (struct_declaration)
        (enum_declaration)
        (union_declaration)
        (opaque_declaration)
        (error_set_declaration)
    ]) @definition.class

; Constants at file and container scope. A `const` inside a `block` is a local binding and is
; deliberately excluded, because in Zig every local is spelled with the same node. Enumerating the
; scopes is the pattern-shape discriminator; the capture still sits on the declaration itself, so
; the span is the declaration and not the enclosing container.
(source_file
    (variable_declaration
        .
        (identifier) @name) @definition.constant)

(struct_declaration
    (variable_declaration
        .
        (identifier) @name) @definition.constant)

(union_declaration
    (variable_declaration
        .
        (identifier) @name) @definition.constant)

(enum_declaration
    (variable_declaration
        .
        (identifier) @name) @definition.constant)

(opaque_declaration
    (variable_declaration
        .
        (identifier) @name) @definition.constant)

; Functions declared inside a container are methods. As in the Rust query, "sits inside a container"
; is expressed by the pattern SHAPE and the capture stays on `function_declaration`, which owns
; `body:`. Putting it on the container would give every method the whole type's span and corrupt
; enclosing attribution for every call in the body.
[
    (struct_declaration
        (function_declaration
            name: (identifier) @name) @definition.method)
    (union_declaration
        (function_declaration
            name: (identifier) @name) @definition.method)
    (enum_declaration
        (function_declaration
            name: (identifier) @name) @definition.method)
    (opaque_declaration
        (function_declaration
            name: (identifier) @name) @definition.method)
]

; Free functions. `name:` is required here because a bare return type (`fn init(…) Point {`) is also
; a direct `identifier` child of the declaration.
(function_declaration
    name: (identifier) @name) @definition.function

; Struct fields, enum members, and union variants are all `container_field`.
(container_field
    name: (identifier) @name) @definition.field

; Error-set members have no node of their own, so the definition capture sits on the identifier.
; That is exact rather than a compromise: the member has no body, and capturing the enclosing
; `error_set_declaration` would give every member the span of the whole set.
(error_set_declaration
    (identifier) @name @definition.constant)

; `test "…" { … }` is a named executable unit that `zig test` addresses by that name.
(test_declaration
    (string
        (string_content) @name)) @definition.function

; ---- references ----

; `@import("std")` is Zig's only import form. The `#eq?` gate is what keeps `@compileError("…")`
; and friends out; a consumer that ignores text predicates degrades to naming those strings, never
; to a wrong edge, because the capture is an import and not a call.
(builtin_function
    (builtin_identifier) @_builtin
    (arguments
        (string
            (string_content) @name))
    (#eq? @_builtin "@import")) @reference.import

; Plain application: `area(shape)`.
(call_expression
    function: (identifier) @name) @reference.call

; The dominant Zig call form is a dotted path: `math.sqrt(…)`, `std.debug.print(…)`,
; `origin.magnitude()`. The `object:` alternation binds @qualifier for the single-segment receiver
; and still matches the multi-segment one; the two branches are disjoint node kinds, so an ordinary
; `a.b()` matches exactly once.
(call_expression
    function: (field_expression
        object: [
            (identifier) @qualifier
            (field_expression)
        ]
        member: (identifier) @name)) @reference.call
