; Kotlin tags — for `tree-sitter-kotlin-ng` (the `-ng` rewrite, not fwcd/tree-sitter-kotlin: node
; kinds differ). Every kind and field below was read off that crate's src/node-types.json and
; confirmed against an AST dump of tests/fixtures/sample.kt.
;
; Deliberate limitations:
;   - Secondary constructors carry no name of their own (the node's only "name" is the
;     `constructor` keyword), while a Kotlin call site spells construction with the CLASS name —
;     already captured by the call patterns. A def named `constructor` would be a symbol no call
;     ever resolves to, so it is skipped.
;   - A local `val`/`var` in a function body is also a `property_declaration`, so the constant and
;     field patterns are anchored under `source_file` / `class_body` instead of matching anywhere.
;     Locals are noise, not symbols.
;   - No `@qualifier`: `a.b()`, `a.b.c()` and `foo().bar()` are one `navigation_expression` shape,
;     so a receiver-only variant would double-capture every qualified call rather than refine it.

; ---- definitions ----

; `class` and `interface` share one node kind; the keyword token is the only discriminator.
; `fun interface` (SAM conversion) also carries the `interface` token, so it lands here too.
(class_declaration
  "interface"
  name: (identifier) @name) @definition.interface

; plain / `data` / `enum` / `sealed` / `value` class — the modifiers sit in a `modifiers` child
(class_declaration
  "class"
  name: (identifier) @name) @definition.class

(object_declaration
  name: (identifier) @name) @definition.class

; `companion object Named { .. }`. The name is optional in the grammar; an anonymous companion has
; no token to name it, so only the named spelling is a symbol. Its members are still captured by
; the `class_body` method pattern below.
(companion_object
  name: (identifier) @name) @definition.class

(type_alias
  type: (identifier) @name) @definition.type

; A function inside any class / interface / object / companion body is a method. The capture sits on
; the `function_declaration`, which owns `function_body`; putting it on `class_body` would give every
; method the span of the whole class and collapse enclosing attribution onto one arbitrary member.
(class_body
  (function_declaration
    name: (identifier) @name) @definition.method)

(function_declaration
  name: (identifier) @name) @definition.function

; `enum class Status { ACTIVE, RETIRED }` — one def per entry, like C enumerators.
(enum_class_body
  (enum_entry
    (identifier) @name) @definition.constant)

; top-level `const val` / `val` / `var`
(source_file
  (property_declaration
    (variable_declaration
      (identifier) @name)) @definition.constant)

; member properties
(class_body
  (property_declaration
    (variable_declaration
      (identifier) @name)) @definition.field)

; `data class Item(val id: String, ..)` — a primary-constructor parameter is a property only when it
; is spelled `val`/`var`; a bare parameter is not, and the keyword token is the whole discriminator.
(class_parameter
  ["val" "var"]
  (identifier) @name) @definition.field

; ---- references ----

; `import a.b.C` / `import a.b.C as D` — `user_type` and `qualified_identifier` are FLAT here
; (`sep1(segment, '.')`), so the trailing anchor picks the imported leaf. `import a.b.*` names the
; package instead, which is the most specific thing that spelling states.
(import
  (qualified_identifier
    (identifier) @name .)) @reference.import

; `foo(..)`, `Item(..)`, `foo { .. }` — the callee is the first child of the call
(call_expression
  . (identifier) @name) @reference.call

; `a.b()`, `a.b.c()`, `items.filter { .. }.joinToString(..)` — Kotlin's dominant call form. The
; receiver is the first child and the invoked name is the last, hence the trailing anchor.
(call_expression
  . (navigation_expression
      (identifier) @name .)) @reference.call

; `: Base()` / `: Contract` / `: Contract<T>` in a supertype list. `user_type` is flat, so the leaf
; segment is last for a plain type and immediately before `type_arguments` for a generic one.
(delegation_specifier
  [(user_type (identifier) @name .)
   (user_type (identifier) @name . (type_arguments))
   (constructor_invocation (user_type (identifier) @name .))
   (constructor_invocation (user_type (identifier) @name . (type_arguments)))]) @reference.extends
