; Workcell HTML tags — written against tree-sitter-html 0.23.2 (`grammar()` returns
; `tree_sitter_html::LANGUAGE`), verified against an AST dump of tests/fixtures/sample.html with
; `cargo run -p workcell-source-languages --example dump`.
;
; HTML is a PROSE lane (`Language::Html.family() == LanguageFamily::Prose`). It emits NO
; `@reference.call` captures and never will: markup declares structure, not control flow, and an
; edge minted from an element would assert a call that no execution performs. `<script>` bodies are
; opaque `raw_text` to this grammar, so the JavaScript inside is not parsed here either — the call
; graph for that code comes from a real `.js`/`.ts` file, never from a smuggled HTML pattern.
;
; What becomes navigable, all of it `@definition.section`:
;   - any element carrying `id=` → the id is the symbol. This is the addressable unit: it is what a
;     fragment URL, a CSS `#rule`, and `getElementById` all name.
;   - h1..h6 and <title> → the heading TEXT is the symbol, matching the markdown lane's posture
;     that a reader navigates by what the heading says, not by which level it is.
;   - sectioning and document landmarks (<section>, <nav>, <main>, ...) plus <script>/<style>,
;     which are distinct node kinds in this grammar → named by their tag.
;
; Deliberate limitations, each forced by the grammar rather than chosen:
;   - An id-bearing landmark is captured TWICE, once under its id and once under its tag. A tags
;     query has no negation, so "landmark WITHOUT an id" is not expressible. Two aliases for one
;     span is the honest outcome: both spellings are ones a reader navigates by.
;   - A heading whose content is wrapped in another element (`<h2><span>..</span></h2>`) has no
;     direct `text` child and is skipped. Capturing the wrapper would name the section after the
;     span's markup, which is worse than omitting it.
;   - `<a href="#anchor">` is NOT captured. The `@name` would carry the leading `#` and so would
;     never resolve to the id definition it points at; @name must be a real token and this grammar
;     gives no token with the `#` stripped.
;   - Custom elements and framework attributes (`v-*`, `:prop`, `@click`) are ordinary attributes
;     to this grammar and get no special treatment.
;
; Element definitions sit on the `element` / `script_element` / `style_element` node, which owns the
; body, never on the `start_tag` wrapper — a `start_tag` span stops at `>` and would report a
; section with no content in it.

; ---- definitions: any element addressed by an id ----

; `<main id="app-shell">` — the id is the symbol, whichever tag carries it.
(element
  (start_tag
    (attribute
      (attribute_name) @_attribute
      (quoted_attribute_value (attribute_value) @name)))
  (#eq? @_attribute "id")) @definition.section

; `<input id="query" />` — void and self-closed elements carry the same attribute shape.
(element
  (self_closing_tag
    (attribute
      (attribute_name) @_attribute
      (quoted_attribute_value (attribute_value) @name)))
  (#eq? @_attribute "id")) @definition.section

(script_element
  (start_tag
    (attribute
      (attribute_name) @_attribute
      (quoted_attribute_value (attribute_value) @name)))
  (#eq? @_attribute "id")) @definition.section

(style_element
  (start_tag
    (attribute
      (attribute_name) @_attribute
      (quoted_attribute_value (attribute_value) @name)))
  (#eq? @_attribute "id")) @definition.section

; ---- definitions: headings, named by the text a reader sees ----

(element
  (start_tag (tag_name) @_tag)
  (text) @name
  (#any-of? @_tag "h1" "h2" "h3" "h4" "h5" "h6" "title" "figcaption" "legend" "summary"))
  @definition.section

; ---- definitions: sectioning and document landmarks, named by their tag ----

(element
  (start_tag (tag_name) @name)
  (#any-of? @name
    "html" "head" "body" "main" "header" "footer" "nav" "section" "article" "aside"
    "form" "figure" "dialog" "template" "noscript")) @definition.section

; `<script>` and `<style>` are their own node kinds here because their bodies are raw text.
(script_element (start_tag (tag_name) @name)) @definition.section

(style_element (start_tag (tag_name) @name)) @definition.section

; ---- references: external resources this document pulls in ----
;
; Imports, not calls. A prose lane may name a dependency; it may not claim an invocation.

(script_element
  (start_tag
    (attribute
      (attribute_name) @_attribute
      (quoted_attribute_value (attribute_value) @name)))
  (#eq? @_attribute "src")) @reference.import

(element
  (self_closing_tag
    (tag_name) @_tag
    (attribute
      (attribute_name) @_attribute
      (quoted_attribute_value (attribute_value) @name)))
  (#any-of? @_tag "link" "img" "iframe" "source" "embed" "track" "object")
  (#any-of? @_attribute "href" "src" "data")) @reference.import

; The same void elements written without the closing slash parse as a bare `start_tag`.
(element
  (start_tag
    (tag_name) @_tag
    (attribute
      (attribute_name) @_attribute
      (quoted_attribute_value (attribute_value) @name)))
  (#any-of? @_tag "link" "img" "iframe" "source" "embed" "track" "object")
  (#any-of? @_attribute "href" "src" "data")) @reference.import
