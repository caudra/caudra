; Elixir tags — for `tree-sitter-elixir`. Elixir has no declaration syntax: `defmodule`, `def` and
; `defmacro` are macros, so the grammar models every one of them as a `call` node whose `target:` is
; an `identifier` holding the keyword. Every definition below is therefore a `call` pattern gated by
; a `#eq?` / `#any-of?` on that keyword, verified against an AST dump of tests/fixtures/sample.ex.
;
; The call patterns are keyed on the PARENT context rather than written as one blanket
; `(call target: (identifier)) @reference.call`, for two reasons:
;   1. A definition head is itself a call — `def restock(a, b)` contains the node `restock(a, b)` —
;      so a blanket pattern mints a call to `restock` at the line that DEFINES `restock`, i.e. a
;      self-edge on every function that takes arguments. Each context below either cannot contain a
;      definition head, or excludes it with the `def`-family predicate.
;   2. Each context names one parent node kind, and a node has exactly one parent, so the patterns
;      cannot double-capture the same call.
; Local calls additionally require a parenthesised argument list. That single structural gate is
; what separates a function call from the macro/directive/special-form spellings that share the
; `call` node kind (`alias Foo`, `require Logger`, `defstruct k: v`, `use Foo`, `if x do`,
; `case x do`, `@attr value`), and it costs only the parenless call spelling that Elixir's own
; style guides tell you not to write.
;
; Deliberate limitations:
;   - `defstruct` and `defimpl` produce no definition. `defstruct`'s field names are `keyword`
;     tokens whose text carries the trailing `": "`, so they cannot be captured as a clean `@name`,
;     and `defimpl` names the protocol, not a new symbol.
;   - `alias Foo.{Bar, Baz}` (the brace-expansion form) is not captured; its argument is a `dot`
;     over a `tuple`, so there is no single alias node to name.
;   - `@doc` is not wired to `@doc`: the attribute is a SIBLING of the definition it documents, and
;     a sibling pattern would break on any blank line or comment between the two.

; ---- definitions ----

; `defmodule Inventory.Catalog do .. end`. The capture is on the outer `call`, which owns the
; `do_block`; the module span containing its functions is the same containment a class gives.
(call
  target: (identifier) @_kw
  (arguments (alias) @name)
  (do_block)
  (#eq? @_kw "defmodule")) @definition.module

(call
  target: (identifier) @_kw
  (arguments (alias) @name)
  (do_block)
  (#eq? @_kw "defprotocol")) @definition.interface

; `def new do .. end`, `def restock(a, b) do .. end`, `def count(c), do: expr`, and the guarded
; `defp normalize(sku) when is_binary(sku) do .. end`. The head spelling differs per form: a bare
; identifier for zero arity, a nested `call` when there are parameters, and a `when` operator whose
; LEFT side is that head. The capture stays on the outer `call`, which spans the whole clause.
(call
  target: (identifier) @_kw
  (arguments
    [(identifier) @name
     (call target: (identifier) @name)
     (binary_operator
       left: [(identifier) @name (call target: (identifier) @name)]
       operator: "when")])
  (#any-of? @_kw "def" "defp" "defdelegate")) @definition.function

(call
  target: (identifier) @_kw
  (arguments
    [(identifier) @name
     (call target: (identifier) @name)
     (binary_operator
       left: [(identifier) @name (call target: (identifier) @name)]
       operator: "when")])
  (#any-of? @_kw "defmacro" "defmacrop" "defguard" "defguardp")) @definition.macro

; `@default_page_size 50` — module attributes are Elixir's constant. The reserved attributes are
; documentation, typing and behaviour declarations rather than values, so they are excluded by name;
; `@behaviour` is captured further down as a conformance reference instead.
(unary_operator
  operator: "@"
  operand: (call
    target: (identifier) @name
    (arguments))
  (#not-any-of? @name
    "moduledoc" "doc" "typedoc" "shortdoc" "spec" "type" "typep" "opaque" "callback"
    "macrocallback" "behaviour" "behavior" "impl" "derive" "enforce_keys" "deprecated"
    "compile" "dialyzer" "external_resource" "before_compile" "after_compile" "on_definition"
    "on_load" "tag" "moduletag" "describetag")) @definition.constant

; ---- references ----

; `alias Inventory.Item`, `require Logger`, `import Enum`, `use GenServer`
(call
  target: (identifier) @_kw
  (arguments (alias) @name)
  (#any-of? @_kw "alias" "import" "require" "use")) @reference.import

; `@behaviour Inventory.Store` — Elixir has no inheritance; a behaviour attribute is the language's
; conformance clause, so it is the honest source of an extends edge.
(unary_operator
  operator: "@"
  operand: (call
    target: (identifier) @_kw
    (arguments (alias) @name))
  (#eq? @_kw "behaviour")) @reference.extends

; `Map.update(..)`, `Enum.sum()`, `String.trim(sku)`, `Item.render()` — the dominant Elixir call
; form. No context gate is needed: a definition head is never a dot call, and `Foo.bar` without
; arguments is a plain `dot`, not a `call`, so bare field-style access cannot reach this pattern.
(call
  target: (dot
    right: (identifier) @name)) @reference.call

; `x |> normalize` / `x |> Enum.sum` — a parenless pipeline stage is a `dot` or an `identifier`
; rather than a `call`, so the pipe operator itself is the evidence that it is invoked.
(binary_operator
  operator: "|>"
  right: [(identifier) @name
          (dot right: (identifier) @name)]) @reference.call

; ---- local calls, by parent context (see the header for why this is not one blanket pattern) ----

; statement position: `log_units(units)` directly inside a `do` block
(do_block
  (call
    target: (identifier) @name
    (arguments "(")) @reference.call)

; a `->` clause body: every `case` / `cond` / `with` / `fn` branch
(body
  (call
    target: (identifier) @name
    (arguments "(")) @reference.call)

; `updated = normalize(sku)`, `x |> helper()`, and the guard in `def f(x) when is_binary(x)`. Only
; the RIGHT operand: a guarded definition head is the LEFT operand of its `when`.
(binary_operator
  right: (call
    target: (identifier) @name
    (arguments "(")) @reference.call)

; `def count(catalog), do: total(catalog)` — the one-line body is a keyword-list value
(pair
  value: (call
    target: (identifier) @name
    (arguments "(")) @reference.call)

; an argument of another call: `Map.update(items, normalize(sku), ..)`, `case total(catalog) do`.
; The predicate is what keeps a definition head — which is exactly a call inside the arguments of
; `def` — from being read as a call to the function being defined.
(call
  target: (_) @_owner
  (arguments
    (call
      target: (identifier) @name
      (arguments "(")) @reference.call)
  (#not-any-of? @_owner
    "def" "defp" "defdelegate" "defmacro" "defmacrop" "defguard" "defguardp" "defn" "defnp"))
