; Workcell Nix tags — written against the vendored `tree-sitter-nix` 0.3.0 crate. Every node kind
; and field below was read off an AST dump of tests/fixtures/sample.nix, not from memory.
;
; ── NIX IS A DATA LANE, AND EMITS NO CALL EDGES ───────────────────────────────────────────────
; Nix is `LanguageFamily::Config`. That is a deliberate classification, not a gap: this query
; captures ONLY `@definition.section` and emits NO `@reference.call` — and no reference of any
; other kind either. A `.nix` file is read the way a `Cargo.toml` is read: the navigable unit is
; the attribute path, not the reduction that computes its value.
;
; This is where the file departs from `queries/tags.scm` shipped inside the grammar crate, which
; mints `@definition.function` for a binding whose value is a lambda and `@reference.call` for a
; curried `apply_expression`. Both patterns are dropped here on purpose. Section symbols are
; declared non-callable (`SymbolKind::callable`), so a call edge in this lane would have no legal
; target, and `Language::compatible_with` already refuses to let a Nix `buildInputs` resolve a
; Rust or Python symbol of the same spelling. Emitting the edge anyway would assert an invocation
; the map can neither target nor honestly rank.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────
;   binding `a = v;`      → a section at ANY depth: attrset, `rec` attrset, `let`, and a module
;                           body are one `binding` shape, so one pattern covers all four. The def
;                           node is the whole `binding`, so expanding `meta` yields its value.
;   dotted attrpath       → ONE section under its FULL spelling (`packages.workcell`), the TOML
;                           dotted-key posture. `attrpath` is a single node whose dots are
;                           children, so splitting is not free here, and a bare `workcell` would
;                           collide with every other leaf of the same name in the file.
;   `inherit a b;`        → one section per attr. `inherit` and `inherit_from`
;                           (`inherit (pkgs) stdenv;`) share the `inherited_attrs` child, so one
;                           pattern covers both. The def sits on the identifier because an
;                           inherited attr owns no body, and `inherited_attrs` is the list
;                           wrapper — capturing that would give every attr the same span.
;   top-level formals     → the file's declared INPUTS (`{ pkgs, lib, ... }:`), which are a
;                           module's public interface. Anchored under `source_code` so that
;                           lambda parameters deeper in the file — `{ name, src }:` bound to a
;                           helper — stay out. The def is the `formal`, which owns its default.
;
; ── DELIBERATE LIMITATIONS ────────────────────────────────────────────────────────────────────
;   · `with pkgs;`, `if`/`assert` bodies, interpolations, and list elements are values, never
;     symbols. A name that only ever appears inside one is invisible to this lane.
;   · `import ./foo.nix` is not captured. Distinguishing it from any other application of a path
;     needs an `#eq?` on the function name, and no query in this crate relies on predicates
;     running, so the pattern is omitted rather than written unsound.
;   · A formal's ellipsis (`...`) is not a name and yields nothing.

; ---- every binding, at any depth: attrset, rec attrset, let, module body ----
(binding
  attrpath: (attrpath) @name) @definition.section

; ---- `inherit a b;` and `inherit (pkgs) a b;` — one section per attr, def on the identifier ----
(inherited_attrs
  attr: (identifier) @name @definition.section)

; ---- the file's own inputs: `{ pkgs ? …, lib ? … }:` at the top level only ----
(source_code
  expression: (function_expression
    formals: (formals
      (formal
        name: (identifier) @name) @definition.section)))
