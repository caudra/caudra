; Workcell Starlark tags — written against the vendored `tree-sitter-starlark` 1.3.0 crate. One
; query serves all three Bazel file shapes, because `BUILD.bazel`, `MODULE.bazel`, and `*.bzl`
; are the same grammar; `Language::from_path` is what tells them apart, and
; `Language::compatible_with` already lets the three resolve each other. Node kinds below were
; read off AST dumps of tests/fixtures/{BUILD.bazel,MODULE.bazel,sample.bzl}, not from memory.
;
; ── STARLARK IS A CODE LANE ───────────────────────────────────────────────────────────────────
; Unlike the Nix and HCL data lanes, Bazel files execute: `LanguageFamily::Code`. A BUILD file is
; a sequence of macro and rule INVOCATIONS, so `cc_library(…)`, `load(…)`, and `bazel_dep(…)` are
; genuine call sites, and the `.bzl` files they load define the functions being called. Dropping
; those edges would leave the most connected files in a Bazel repository looking edgeless.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────
;   `def f(…)`            → `@definition.function`. The `.bzl` surface: rule implementations and
;                           macros. The def is the `function_definition`, which owns the body.
;   module assignment     → `@definition.constant`: `DEFAULT_COPTS = […]`, and equally
;                           `workcell_binary = rule(…)` and `WorkcellInfo = provider(…)`, which
;                           are how a `.bzl` file exports a rule or a provider. Anchored under
;                           `module` so a local inside a function body is not a symbol.
;   BUILD target          → `@definition.section` named by the target's `name = "…"`, qualified
;                           by the rule that declares it. A target is the navigable unit of a
;                           BUILD file — `//pkg:logging` is what `deps` points at — but nothing
;                           CALLS it, so it must not be a callable kind: `SymbolKind::Section` is
;                           the only kind declared non-callable, which is exactly the property
;                           wanted here. The def node is the whole `call`, so expanding a target
;                           yields its attributes.
;   any call              → `@reference.call`, named by the identifier or, for `ctx.actions.run`
;                           and `rust.toolchain`, by the final attribute. This is the Python
;                           query's call pattern, and it is what makes a rule invocation resolve
;                           to the `def` that a `load()` brought in.
;
; ── HOW THE TARGET PATTERN IS GATED, AND WHAT THAT COSTS ──────────────────────────────────────
; `name` is matched STRUCTURALLY, by position, not by an `#eq?` on the keyword: no query in this
; crate relies on predicates running. The pattern requires the FIRST named argument to be a
; keyword argument whose value is a string LITERAL — which is buildifier's canonical rule order
; and near-universal in real BUILD and MODULE files.
;   · It excludes `load("@rules_cc//cc:defs.bzl", …)` and `register_toolchains("//x:all")`, whose
;     first argument is positional, and `package(default_visibility = […])`, whose first value is
;     a list, and `rust.toolchain(edition = "2021")`, whose function is an attribute rather than
;     a bare identifier — all three by shape, none by name.
;   · It costs precision on a rule written with `name` NOT first, which is dropped, and on a
;     non-rule top-level call whose first argument happens to be a string keyword, which is
;     minted as a target. Both are rare in buildifier-formatted files and neither can be
;     distinguished from the wanted shape without a predicate.
;
; ── DELIBERATE LIMITATIONS ────────────────────────────────────────────────────────────────────
;   · `load()` records as a CALL, not `@reference.import`, and the `.bzl` label and symbols it
;     names are not captured. Splitting it out needs an `#eq?` on the callee.
;   · A target whose `name` is computed (`name = name + "_test"`) has no string literal and is
;     dropped rather than named with a fragment.
;   · Rules declared inside a macro body are calls, not targets: the target pattern is anchored
;     at `module`, because a macro's declarations belong to whichever BUILD file calls it.
;   · Docstrings are not captured. tree-sitter has no optional child pattern, so a `@doc` arm
;     would need a second `function_definition` pattern and would double-count every documented
;     function.

; ---- `def` functions: rule implementations and macros in a .bzl file ----
(function_definition
  name: (identifier) @name) @definition.function

; ---- module-level bindings: constants, and the `rule(…)` / `provider(…)` a .bzl file exports ----
(module
  (expression_statement
    (assignment
      left: (identifier) @name) @definition.constant))

; ---- a declared Bazel target: top-level rule call whose first argument is `name = "<literal>"` ----
(module
  (expression_statement
    (call
      function: (identifier) @qualifier
      arguments: (argument_list
        .
        (keyword_argument
          name: (identifier)
          value: (string (string_content) @name)))) @definition.section))

; ---- every invocation: rule and macro calls in BUILD/MODULE, helper calls inside .bzl ----
(call
  function: [
    (identifier) @name
    (attribute
      attribute: (identifier) @name)
  ]) @reference.call
