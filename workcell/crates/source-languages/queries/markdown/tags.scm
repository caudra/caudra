; Workcell Markdown tags — written against tree-sitter-md 0.5.3, and specifically against
; `tree_sitter_md::LANGUAGE`, which is the BLOCK grammar. tree-sitter-md ships two grammars, block
; and inline, and this crate loads only the block one. A heading's text therefore arrives as a
; single opaque `inline` node that is never sub-parsed, so the captured name is the RAW SOURCE of
; the heading line: "## The `index` tool" names the symbol with its backticks intact. That is a
; deliberate limitation, disclosed here rather than papered over — stripping markup this build
; cannot see would mean inventing a name that is not a token in the tree.
;
; Markdown is a PROSE lane (LanguageFamily::Prose). It emits @definition.section and NOTHING else.
; There are no @reference.call patterns in this file and there never may be: a heading is not a call
; site, and an edge minted from one would assert control flow that no execution performs. It is the
; same rule that stops a TOML key from resolving a Rust function of the same spelling.
;
; ── WHY THE CAPTURE SITS ON THE HEADING AND NOT ON THE ENCLOSING `section` ────────────────────────
; The block grammar wraps an ATX heading and the blocks after it in a `section`, and nests deeper
; sections inside shallower ones. Capturing `section` would give the first heading of a document a
; span covering the whole file, and every other heading a span covering all of its descendants.
; It would also be inconsistent between the two heading syntaxes: `setext_heading` gets NO section
; of its own — the grammar emits it as a plain block inside whichever section is already open — so
; setext and ATX headings would report spans of different kinds. One heading, one span, both
; syntaxes. Neither `atx_heading` nor `setext_heading` is a wrapper list node; each is the node the
; heading itself owns, so no span-widening walk has anywhere to climb.
;
; An ATX heading with no text (`##` alone) has no `heading_content` field and so yields no symbol.
; There is no name token to carry, and a definition with no @name cannot be addressed or resolved.

; ---- ATX headings: `# Title` through `###### Title` ----
(atx_heading
  heading_content: (inline) @name) @definition.section

; ---- setext headings: a paragraph underlined with `===` or `---` ----
; `heading_content` is the whole `paragraph`, whose text includes the trailing newline, so the name
; is taken from the `inline` inside it and the two heading syntaxes produce comparable names.
(setext_heading
  heading_content: (paragraph
    (inline) @name)) @definition.section
