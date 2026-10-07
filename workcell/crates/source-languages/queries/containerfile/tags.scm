; Workcell Containerfile tags — written against tree-sitter-containerfile 0.9.2
; (`tree_sitter_containerfile::LANGUAGE`), the grammar used for both `Containerfile` and
; `Dockerfile`.
;
; A Containerfile is a CONFIG lane (LanguageFamily::Config). It emits @definition.section and
; NOTHING else. There are no @reference.call patterns in this file and there never may be: a build
; instruction is data describing an image, not an invocation of another symbol, and an edge minted
; from one would assert a call that no execution performs. `RUN cargo build` names a shell command,
; not a symbol this map can resolve, and `COPY --from=builder` is a stage reference the grammar does
; not expose as a token (its `param` node has no named value child), so it is not captured either.
; That posture matches the TOML lane: sections only, no reference captures of any kind.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────────
;   FROM x AS builder → TWO sections on the same instruction, one named `builder` and one named `x`.
;                       The stage alias is the navigable unit of a multi-stage build and the base
;                       image is what a reader greps for; they are different name tokens, so both
;                       survive dedup and neither has to be chosen at the other's expense. The
;                       grammar cannot express "alias if present, else image" — a query has no
;                       negation over an optional field — so emitting one would silently drop the
;                       other.
;   ARG / ENV         → named by the KEY, from the `name:` field of the pair. The value is data.
;   WORKDIR / EXPOSE  → named by the path and the port, the only token each instruction carries.
;   COPY              → named by its DESTINATION, which is the last `path` child (the `.` anchor
;                       below). Capturing every `path` would mint one section per source operand,
;                       so `COPY a b ./` would become three sections describing one instruction.
;   RUN / ENTRYPOINT  → named by the command they carry, in either the shell or the JSON-array form.
;   / CMD               A step named `cargo build --release` is what makes an image build
;                       navigable; naming all of them `RUN` would collapse every step onto one
;                       symbol. The disclosed cost: a line-continued multi-line RUN is named by the
;                       whole continued block, because that block is the single token the grammar
;                       gives.
;   LABEL / ADD / etc → not captured. Only the instructions listed above carry a name worth
;                       navigating to; the rest are left out rather than given a synthetic name.

; ---- build stages: `FROM ... AS builder` ----
(from_instruction
  as: (image_alias) @name) @definition.section

; ---- FROM, named by its base image ----
(from_instruction
  (image_spec
    name: (image_name) @name)) @definition.section

; ---- ARG and ENV, named by the key ----
(arg_instruction
  (arg_pair
    name: (unquoted_string) @name)) @definition.section

(env_instruction
  (env_pair
    name: (unquoted_string) @name)) @definition.section

; ---- WORKDIR and EXPOSE ----
(workdir_instruction
  (path) @name) @definition.section

(expose_instruction
  (expose_port) @name) @definition.section

; ---- COPY, named by its destination: the trailing `.` anchors the capture to the LAST path ----
(copy_instruction
  (path) @name .) @definition.section

; ---- RUN, ENTRYPOINT and CMD, in both the shell and the JSON-array form ----
(run_instruction
  [
    (shell_command)
    (json_string_array)
  ] @name) @definition.section

(entrypoint_instruction
  [
    (shell_command)
    (json_string_array)
  ] @name) @definition.section

(cmd_instruction
  [
    (shell_command)
    (json_string_array)
  ] @name) @definition.section
