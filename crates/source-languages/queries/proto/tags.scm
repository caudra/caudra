; Workcell Protobuf tags — written against tree-sitter-proto 0.6.0 (`tree_sitter_proto::LANGUAGE`),
; used for `.proto`.
;
; Protobuf is a CONFIG lane (LanguageFamily::Config). A schema declares types and services, and
; nothing in it runs, so it emits no call edges: an `rpc` is a method that generated code
; implements, not a call site.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────────
;   message, group, enum → @definition.class, named by the type's own name at any depth, so a
;                        nested `Outer.Inner` is the symbol `Inner`.
;   service            → @definition.interface.
;   rpc                → @definition.method.
;   type reference     → @reference.read, on every field, map value, and rpc request or response
;                        type, named by its last component so `google.protobuf.Timestamp` reads
;                        `Timestamp`.
;   DISCLOSED LIMITATION: an `import` path is a quoted `string` token with no unquoted child, and
;   @name must be a real token, so imports are not captured.

; ---- types ----
(message
  (message_name
    (identifier) @name)) @definition.class

(group
  (message_name
    (identifier) @name)) @definition.class

(enum
  (enum_name
    (identifier) @name)) @definition.class

; ---- services ----
(service
  (service_name
    (identifier) @name)) @definition.interface

(rpc
  (rpc_name
    (identifier) @name)) @definition.method

; ---- type references ----
(message_or_enum_type
  (identifier) @name .) @reference.read
