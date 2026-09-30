; Workcell XML tags — written against tree-sitter-xml 0.7.0 (`tree_sitter_xml::LANGUAGE_XML`), used
; for `.xml` and the XML formats routed to it: `.xsd`, `.xsl`, `.xslt`, `.xaml`, `.plist`, `.resx`,
; `.wsdl`, and MSBuild's `.csproj`, `.fsproj`, `.vbproj`, `.vcxproj`, `.props`, and `.targets`.
;
; XML is a PROSE lane (LanguageFamily::Prose), as HTML is. Markup declares structure, not control
; flow, so it emits no call references.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────────
;   root element       → @definition.section, named by its tag.
;   its child elements → @definition.section, named by their tags. Deeper elements are not symbols:
;                        a document's outline is its first two levels, and every `<dependency>` of
;                        a build file as a symbol would bury the map in repeats.
;   DISCLOSED LIMITATION: an `AttValue` token includes its quotes, and @name must be a real token,
;   so an element is never named by its `id` or `name` attribute and no attribute is an import.

; ---- the root element ----
(document
  root: (element
    [
      (STag (Name) @name)
      (EmptyElemTag (Name) @name)
    ]) @definition.section)

; ---- the root's child elements ----
(document
  root: (element
    (content
      (element
        [
          (STag (Name) @name)
          (EmptyElemTag (Name) @name)
        ]) @definition.section)))
