; Workcell HCL tags — written against the vendored `tree-sitter-hcl` 1.1.0 crate, and covering
; every extension the table routes here: `.hcl`, `.tf`, and `.tfvars`. Node kinds and the label
; arities below were read off an AST dump of tests/fixtures/sample.tf, not from memory.
;
; ── HCL IS A DATA LANE, AND EMITS NO CALL EDGES ───────────────────────────────────────────────
; HCL is `LanguageFamily::Config`. This query captures ONLY `@definition.section` and emits NO
; `@reference.call` — and no reference of any other kind. That holds even though the grammar has
; a first-class `function_call` node and a real one appears in the fixture
; (`merge(local.common_tags, …)`): `merge` is an interpreter builtin, not a symbol any file in
; the map defines, so an edge to it could never resolve. Section symbols are declared
; non-callable (`SymbolKind::callable`) and `Language::compatible_with` already refuses to let an
; HCL key resolve a code symbol of the same spelling, so the edge would be unresolvable AND
; unrankable. Terraform's own dependency graph lives in `var.x` / `aws_s3_bucket.logs.id`
; traversals, which are reads rather than invocations, and are left out as a limitation below
; rather than smuggled in as calls.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────
; A `block` is `identifier`, then 0, 1, or 2 quoted labels, then `block_start body block_end`.
; The grammar names no fields on it, so arity is matched STRUCTURALLY with the anchor operator:
; `block_start` is a NAMED node, so `. (block_start)` after the last label means "nothing else
; between", and the three patterns below are mutually exclusive without a single predicate.
;   0 labels  `terraform {}`  → section named by the block type. Also covers `locals` and nested
;                               header-less blocks such as `versioning_configuration`.
;   1 label   `variable "x"`  → section named by the LABEL, qualified by the block type. That is
;                               the addressable half: a user greps `bucket_prefix`, not
;                               `variable`.
;   2 labels  `resource "aws_s3_bucket" "logs"` → section named `logs`, qualified
;                               `aws_s3_bucket`. Terraform's address for the block is
;                               `<type>.<name>`, which is exactly qualifier + name; the `resource`
;                               keyword is a category, not part of the address, so it is not the
;                               name. The def node is the whole `block`, so expanding a header
;                               yields its body.
;   attribute `a = v`         → a section at ANY depth, the TOML key posture. `.tfvars` files are
;                               nothing BUT root-level attributes, so a block-only rule would
;                               make them extract nothing.
;   quoted labels             → quotes STRIPPED: `string_lit` wraps a `template_literal` child
;                               that holds the text, unlike TOML's `quoted_key`, so `logs` is the
;                               name rather than `"logs"`.
;
; ── DELIBERATE LIMITATIONS ────────────────────────────────────────────────────────────────────
;   · `object_elem` keys inside an `{ … }` value are NOT descended; the owning attribute is the
;     symbol. Same line TOML draws at an inline table. So `required_providers`'s `aws` is a
;     symbol (it is an `attribute`) while the `source`/`version` pairs inside its object value
;     are not.
;   · No reference captures at all, so `var.region` and `aws_s3_bucket.logs.id` do not become use
;     sites. Terraform's cross-resource graph is therefore not modelled here.
;   · A label spelled with an interpolation (`"${var.env}"`) has no `template_literal` child and
;     yields no name, so the block is dropped rather than named with a fragment.
;   · `for`/`dynamic` expression bodies and heredocs are values, never symbols.

; ---- block with no labels: `terraform { }`, `locals { }`, `versioning_configuration { }` ----
(block
  (identifier) @name
  .
  (block_start)) @definition.section

; ---- one label: `variable "bucket_prefix" { }`, `provider "aws" { }`, `output "x" { }` ----
(block
  (identifier) @qualifier
  .
  (string_lit (template_literal) @name)
  .
  (block_start)) @definition.section

; ---- two labels: `resource "aws_s3_bucket" "logs" { }` — named `logs`, qualified by the type ----
(block
  (identifier)
  .
  (string_lit (template_literal) @qualifier)
  .
  (string_lit (template_literal) @name)
  .
  (block_start)) @definition.section

; ---- every attribute, at any depth, including a bare `.tfvars` root ----
(attribute
  (identifier) @name) @definition.section
