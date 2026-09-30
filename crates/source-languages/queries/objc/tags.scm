; Seeded from ripwire (Apache-2.0), which derived it from the upstream tree-sitter-objc highlights
; query (MIT) and a verified parse dump. Re-verified against tree-sitter-objc 3.0.2
; (`tree_sitter_objc::LANGUAGE`), used for `.m` and `.mm`; capture names are normalized to the
; Workcell vocabulary in src/roles.rs.
;
; The grammar extends tree-sitter-c, and src/queries.rs appends this file to queries/c/tags.scm, so
; the C patterns capture every C function, type, macro, constant, field, and call in an Objective-C
; file. This file holds the Objective-C layer only. ripwire's C function and call patterns are
; dropped rather than repeated: references are not deduplicated, so a repeated call pattern would
; mint every C call twice.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────────
;   @interface, @implementation → @definition.class, named by the identifier after the keyword. A
;                        category (`@interface Widget (Drawing)`) names the class it extends, so each
;                        category adds one more symbol under that class's name.
;   @protocol          → @definition.interface.
;   method             → @definition.method, declared or defined, named by the first selector
;                        keyword: `initWithTitle:count:` is `initWithTitle`, one symbol per method.
;   @property          → @definition.property (Workcell addition), named by its declarator.
;   message send       → @reference.call, named by the first selector keyword so that it meets the
;                        method's name. Divergence from ripwire, which minted a reference per keyword
;                        and so sent the `count:` of `initWithTitle:count:` to whatever method or
;                        property is named `count`.
;   @import            → @reference.import (Workcell addition), named by the module's first
;                        component. `#import` is the C include and, like it, is not captured.
;   DISCLOSED LIMITATION: the grammar does not know C++. An Objective-C++ `.mm` file parses its
;   Objective-C and C, and error-recovers around templates, namespaces, and `::`. Nor does it know
;   Foundation's unterminated macros: `NS_ENUM` typedefs define no type, and the recovery after
;   `NS_ASSUME_NONNULL_BEGIN` can swallow the declaration that follows it.

; ---- definitions ----

(class_interface "@interface" . (identifier) @name) @definition.class

(class_implementation "@implementation" . (identifier) @name) @definition.class

(protocol_declaration . (identifier) @name) @definition.interface

(method_declaration (method_type) . (identifier) @name) @definition.method

(method_definition (method_type) . (identifier) @name) @definition.method

(property_declaration
  (struct_declaration
    (struct_declarator [
      (identifier) @name
      (pointer_declarator declarator: (identifier) @name)
    ]))) @definition.property

; ---- references ----

(message_expression receiver: (_) . method: (identifier) @name) @reference.call

(module_import . path: (identifier) @name) @reference.import
