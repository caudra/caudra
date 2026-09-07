; Workcell Dart tags — authored against tree-sitter-dart 0.2.0 (node kinds and field names verified
; with `examples/dump` against tests/fixtures/sample.dart, cross-checked in the grammar's
; node-types.json). Upstream ships no tags.scm, so every pattern here is original.
;
; The shape that drives this file: this grammar splits a member into a SIGNATURE node and a
; `function_body` sibling, joined by `method_declaration` / `function_declaration`. Only the joining
; node owns `body:`, so every definition capture sits there and never on `class_member`, whose span
; would be the same but which is a list wrapper the grammar also uses for annotations and multi-name
; field declarations. An abstract member is the one case with no body at all, and there the
; signature IS the whole declaration.
;
; Deliberate limitations, all soundness rather than effort:
;   * Cascades (`buffer..add(x)..add(y)`) mint no call edge. Their target is a `cascade_selector`
;     against an implicit receiver, so a name lifted from one would claim a call the reader cannot
;     attribute to any receiver visible in the pattern.
;   * `new Foo()` / `const Foo()` mint no edge; the modern spelling is `Foo()`, which is an ordinary
;     `call_expression` and is captured.
;   * An import alias (`as math`) is not captured. It names the local binding, not the imported
;     library, and the vocabulary has no alias role to keep the two apart.
;   * A call whose receiver is an index or a call (`items[0].run()`, `build().step()`) mints no
;     edge: `object:` would need an open wildcard, which overlaps the identifier branch and would
;     double-count every ordinary `a.b()`.

; ---- definitions ----

(class_declaration
    name: (identifier) @name) @definition.class

(enum_declaration
    name: (identifier) @name) @definition.class

; An extension adds members to a type it does not own; it is still the block those members live in.
(extension_declaration
    name: (identifier) @name) @definition.class

; A mixin is Dart's reusable contract, the trait/protocol slot of the vocabulary.
(mixin_declaration
    name: (identifier) @name) @definition.interface

(type_alias
    (type_identifier) @name) @definition.class

(enum_constant
    name: (identifier) @name) @definition.constant

; Top-level functions. `function_declaration` owns `body:`; the `function_signature` inside it is
; also reachable from `method_signature`, so the capture must not sit on the signature.
(function_declaration
    signature: (function_signature
        name: (identifier) @name)) @definition.function

; Members with a body: methods, getters, setters, operators, generative and factory constructors.
; The alternation is over the signature kinds `method_signature` admits, so each spelling
; contributes exactly one name and the span stays the declaration that owns the body.
;
; The constructor branches carry an anchor for a measured reason: this grammar labels BOTH segments
; of `factory Circle.unit()` with the `name:` field, so an unanchored `name: (identifier) @name`
; matches twice and mints a phantom `Circle` method next to the real `unit`. Requiring the captured
; identifier to sit immediately before `parameters:` selects the final segment, and the unnamed
; spelling `factory Circle()` still matches because there the only segment is also the last one.
(method_declaration
    signature: (method_signature
        [
            (function_signature
                name: (identifier) @name)
            (getter_signature
                name: (identifier) @name)
            (setter_signature
                name: (identifier) @name)
            (operator_signature
                operator: [
                    "[]"
                    "[]="
                    "~"
                    (binary_operator)
                ] @name)
            (constructor_signature
                name: (identifier) @name
                .
                parameters: (formal_parameter_list))
            (factory_constructor_signature
                name: (identifier) @name
                .
                parameters: (formal_parameter_list))
        ])) @definition.method

; Members with no body: abstract methods and accessors, and the `Circle(this.radius);` constructor
; whose whole definition is its parameter list. These sit under `declaration`, never under
; `method_declaration`, so this pattern cannot also match a concrete member. The capture is on the
; signature because that is the entire definition here — there is no body to widen to.
(declaration
    [
        (function_signature
            name: (identifier) @name)
        (getter_signature
            name: (identifier) @name)
        (setter_signature
            name: (identifier) @name)
        (constructor_signature
            name: (identifier) @name
            .
            parameters: (formal_parameter_list))
        (constant_constructor_signature
            name: (identifier) @name
            .
            parameters: (formal_parameter_list))
    ] @definition.method)

; Instance fields and class-level constants. The capture sits on the individual declarator, not on
; the `*_list` wrapper, so `final int a = 1, b = 2;` yields two definitions with their own spans.
(declaration
    (initialized_identifier_list
        (initialized_identifier
            name: (identifier) @name) @definition.field))

(declaration
    (static_final_declaration_list
        (static_final_declaration
            name: (identifier) @name) @definition.constant))

(top_level_variable_declaration
    (initialized_identifier_list
        (initialized_identifier
            name: (identifier) @name) @definition.constant))

(top_level_variable_declaration
    (static_final_declaration_list
        (static_final_declaration
            name: (identifier) @name) @definition.constant))

; ---- references ----

; The library URI without its quotes, matching how the CSS query names an `@import` target. The
; quote-style alternation is the grammar's own split: single and double quoted strings are distinct
; node kinds with distinct content kinds.
(import_specification
    uri: (configurable_uri
        (uri
            (string_literal
                [
                    (string_literal_single_quotes
                        (template_chars_single_single) @name)
                    (string_literal_double_quotes
                        (template_chars_double_single) @name)
                ])))) @reference.import

; `show Foo, Bar` / `hide Baz` name imported symbols one identifier at a time, so the reference sits
; on the identifier rather than on the combinator that lists several.
(combinator
    (identifier) @name @reference.import)

; `extends`, `with`, and `implements` are all supertype clauses. `superclass` holds the extended
; type in its `type:` field and the `with` list in a nested `mixins` node, so the two never collide.
(superclass
    type: (type
        (type_identifier) @name) @reference.extends)

(mixins
    (type
        (type_identifier) @name) @reference.extends)

(interfaces
    (type
        (type_identifier) @name) @reference.extends)

; Plain application, which in Dart also covers construction: `scaleAll(…)`, `Circle(1.5)`.
(call_expression
    function: (identifier) @name) @reference.call

; The dominant Dart call form is a member call: `log.add(…)`, `Circle.unit()`,
; `shapes.first.describe()`. The `object:` alternation binds @qualifier for the single-segment
; receiver and still matches the chained one; the branches are disjoint node kinds, so an ordinary
; `a.b()` matches exactly once.
(call_expression
    function: (member_expression
        object: [
            (identifier) @qualifier
            (member_expression)
        ]
        property: (identifier) @name)) @reference.call
