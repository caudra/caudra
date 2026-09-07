; Workcell CSS tags — written against tree-sitter-css 0.25.0 (`grammar()` returns
; `tree_sitter_css::LANGUAGE`), verified against an AST dump of tests/fixtures/sample.css with
; `cargo run -p workcell-source-languages --example dump`.
;
; CSS is a PROSE lane (`Language::Css.family() == LanguageFamily::Prose`). It emits NO
; `@reference.call` captures and never will. That is not an omission to be fixed later: CSS has no
; invocation. `url(..)`, `var(..)`, `calc(..)` and friends parse as `call_expression` with a
; `function_name`, and capturing those as calls would mint call-graph edges to functions that do not
; exist, out of a grammar node that is named after C syntax rather than after CSS semantics.
;
; What becomes navigable, all of it `@definition.section`:
;   - every selector in a rule set. `html, body { .. }` yields TWO sections over one block, because
;     both spellings are ones a reader looks the block up by. The same choice as `@media a, b`.
;   - `@media` / `@supports` — named by their condition, which is the text a reader greps for.
;   - `@keyframes` — named by the animation name, the one place CSS has a real declared identifier.
;   - any other at-rule (`@font-face`, `@page`, `@layer`) — named by its `at_keyword`.
;
; Custom properties are the one genuine definition/use pair in the language, so they are modelled as
; such: `--brand-fg: ..` is a `@definition.constant` and `var(--brand-fg)` is a `@reference.read`.
; The `--` test is a `#match?` predicate, which the Rust binding evaluates during `matches()`;
; without it the pattern would mint a constant for every `margin:` and `color:` in the file.
;
; Deliberate limitations:
;   - `@name` keeps the selector's punctuation (`.panel`, `#app-shell`, `a[href]:hover`). The
;     grammar has no token spanning the selector with its sigil stripped, and the sigil is what
;     distinguishes a class from an id from a tag, so stripping it would merge three different
;     symbols. Cross-language resolution cannot join a CSS `#app-shell` to an HTML `id="app-shell"`
;     in any case: `compatible_with` refuses every cross-language pair outside TS/JS and Bazel.
;   - `@charset` and `@namespace` are their own node kinds with no useful name and are skipped.
;   - Keyframe steps (`from`, `to`, `50%`) are not sections. They are positions inside one
;     animation, not units anyone navigates to.
;   - Declarations other than custom properties are values, not symbols, and are not captured.
;
; Section captures sit on the `rule_set` / `*_statement` node that owns the `block`, never on the
; `selectors` wrapper — a `selectors` span stops before `{` and would report a section containing
; none of its own declarations.

; ---- definitions: rule sets, one section per selector ----

(rule_set
  (selectors
    [
      (universal_selector)
      (nesting_selector)
      (tag_name)
      (class_selector)
      (id_selector)
      (pseudo_class_selector)
      (pseudo_element_selector)
      (attribute_selector)
      (child_selector)
      (descendant_selector)
      (sibling_selector)
      (adjacent_sibling_selector)
      (namespace_selector)
      (string_value)
    ] @name)) @definition.section

; ---- definitions: conditional groups, named by their condition ----

(media_statement
  [
    (keyword_query)
    (feature_query)
    (binary_query)
    (unary_query)
    (selector_query)
    (parenthesized_query)
  ] @name
  (block)) @definition.section

(supports_statement
  [
    (keyword_query)
    (feature_query)
    (binary_query)
    (unary_query)
    (selector_query)
    (parenthesized_query)
  ] @name
  (block)) @definition.section

; ---- definitions: named animations and remaining at-rules ----

(keyframes_statement (keyframes_name) @name) @definition.section

; `@font-face`, `@page`, `@layer`, `@container` … the keyword itself is the only name available.
(at_rule (at_keyword) @name) @definition.section

; ---- definitions: custom properties ----

(declaration (property_name) @name (#match? @name "^--")) @definition.constant

; ---- references: stylesheet imports and custom-property reads ----
;
; Imports and reads, never calls. A prose lane may name a dependency; it may not claim an
; invocation.

(import_statement (string_value (string_content) @name)) @reference.import

(import_statement
  (call_expression
    (arguments [ (string_value (string_content) @name) (plain_value) @name ]))) @reference.import

(call_expression
  (function_name) @_function
  (arguments . (plain_value) @name)
  (#eq? @_function "var")) @reference.read
