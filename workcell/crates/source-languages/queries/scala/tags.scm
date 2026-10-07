; Scala tags — for `tree-sitter-scala` (0.25.x node kinds: `class_definition`, `template_body`,
; `field_expression`, `instance_expression`). Kinds and field names were read off that crate's
; src/node-types.json and confirmed against an AST dump of tests/fixtures/sample.scala.
;
; Deliberate limitations:
;   - Infix method calls (`a + b`, `xs map f`) are `infix_expression`, not `call_expression`. They
;     are real invocations, but capturing them would mint an edge for every arithmetic and
;     comparison operator in the file, so operator dispatch is left out of the call graph.
;   - `package a.b` is not a definition here. The clause names a namespace that every file in it
;     repeats, so capturing it would emit one duplicate module symbol per file rather than one
;     symbol per namespace. `package a.b { .. }` is skipped for the same reason.
;   - `given` / `extension` (Scala 3) are not captured: neither has a call-site spelling that a
;     name-based resolver can match, so they would be defs nothing ever resolves to.
;   - A member `val` is `@definition.field` whether it sits in a `class` or an `object`. The
;     grammar exposes no cheap discriminator, and an object's `val` is still its member.

; ---- definitions ----

; `class Foo`, `case class Foo` — the `case` modifier is a token on the same node
(class_definition
  name: (identifier) @name) @definition.class

; `object Foo`, `case object Foo` — a singleton, i.e. the type and its only instance
(object_definition
  name: (identifier) @name) @definition.class

(trait_definition
  name: (identifier) @name) @definition.interface

(enum_definition
  name: (identifier) @name) @definition.class

(type_definition
  name: (type_identifier) @name) @definition.type

; A `def` inside a class / object / trait body is a method. The capture sits on the
; `function_definition`, which owns `body:`; hanging it on `template_body` would give every member
; the span of the whole template and collapse enclosing attribution onto one arbitrary method.
(template_body
  (function_definition
    name: (identifier) @name) @definition.method)

(function_definition
  name: (identifier) @name) @definition.function

; `def lookup(sku: String): Option[BigDecimal]` with no body — a trait's abstract member. This node
; kind occurs only inside a template, so it needs no enclosing pattern to be a method.
(function_declaration
  name: (identifier) @name) @definition.method

; members. A `val` in a block is also a `val_definition`, so both patterns are anchored to
; `template_body`: a local binding is not a symbol.
(template_body
  (val_definition
    pattern: (identifier) @name) @definition.field)

(template_body
  (var_definition
    pattern: (identifier) @name) @definition.field)

; `val currency: String` with no value — a trait's abstract member
(template_body
  [(val_declaration name: (identifier) @name)
   (var_declaration name: (identifier) @name)] @definition.field)

; `case class LineItem(sku: String, ..)` — the parameter list IS the field list
(class_parameters
  (class_parameter
    name: (identifier) @name) @definition.field)

; Scala 3 `enum Status { case Active, Retired }` — `case Circle(r: Double)` is the `full_enum_case`
; spelling and carries its own parameter list.
(enum_case_definitions
  [(simple_enum_case name: (identifier) @name)
   (full_enum_case name: (identifier) @name)] @definition.constant)

; ---- references ----

; `import scala.collection.mutable` — the path is a FLAT run of `identifier`/`.` children, so the
; trailing anchor picks the imported leaf.
(import_declaration
  (identifier) @name .) @reference.import

; `import scala.util.{Failure, Success, Try}` — one reference per selector
(import_declaration
  (namespace_selectors
    (identifier) @name)) @reference.import

; `lookup(..)`, `Success(..)`, `BigDecimal(0)`
(call_expression
  function: (identifier) @name) @reference.call

; `hits.update(..)`, `prices.get(..)`, `items.map(..).foldLeft(..)` — Scala's dominant call form.
; `field_expression` nests left, so `field:` is the invoked name at every chain depth.
(call_expression
  function: (field_expression
    field: (identifier) @name)) @reference.call

; `build[Int](1)` and `mutable.Map.empty[String, Int](..)` — an explicit type application wraps the
; callee in `generic_function` instead of naming it directly.
(call_expression
  function: (generic_function
    function: (identifier) @name)) @reference.call

(call_expression
  function: (generic_function
    function: (field_expression
      field: (identifier) @name))) @reference.call

; `new Catalog(..)` / `new java.util.ArrayList[String]()` — construction resolves to the class
(instance_expression
  [(type_identifier) @name
   (stable_type_identifier (type_identifier) @name)
   (generic_type (type_identifier) @name)
   (generic_type (stable_type_identifier (type_identifier) @name))]) @reference.call

; `extends PriceSource with Mixin` — `type:` repeats across the `with` chain
(extends_clause
  type: (type_identifier) @name) @reference.extends
