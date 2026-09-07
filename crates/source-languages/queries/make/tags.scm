; Workcell Make tags — written against tree-sitter-make 1.1.1 (`tree_sitter_make::LANGUAGE`), used
; for `Makefile`, `GNUmakefile` and `*.mk`.
;
; Make is a CODE lane (LanguageFamily::Code), so unlike the config and prose lanes it does emit call
; edges. A makefile really is a call graph: a target's prerequisites are the targets make invokes
; before it, which is the dependency graph every `make` run walks.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────────
;   rule target        → @definition.function, captured on the `rule` (which owns the recipe body),
;                        named by the `word` inside `targets`. The capture is NOT on `targets`:
;                        that is a wrapper list, and a multi-target rule `a b: dep` must produce two
;                        symbols that each span the rule, not one symbol spanning the target list.
;   variable assign    → @definition.constant, for both `X := v` and the shell form `X != cmd`.
;   define ... endef   → @definition.function, not a constant. It is the only user-defined callable
;                        in make and it is exactly what `$(call ...)` invokes, so the definition
;                        kind is chosen to agree with the reference kind that resolves to it.
;   prerequisite       → @reference.call, on the prerequisite word itself so the use site's span is
;                        the token rather than the whole prerequisite list.
;   $(MAKE)            → @reference.call. Recursive make is a call. DISCLOSED LIMITATION: the
;                        operand — the `-C dir` or the sub-target in `$(MAKE) build` — is plain text
;                        inside `shell_text` with no node of its own, so it cannot be captured. The
;                        recursion SITE is recorded, named `MAKE`; the callee is not knowable here.
;   $(call fn,args)    → @reference.call. DISCLOSED LIMITATION: the grammar's `text` token does not
;                        stop at the argument separator, so `arguments` holds ONE child spelling
;                        `fn,args`, and that is the name this records. A single-argument
;                        `$(call fn)` records exactly `fn`. The alternative — naming the reference
;                        after the `call` builtin itself — would be a true token that carries none
;                        of the callee, and @name cannot be synthesized from a substring.
;   $(VAR)             → @reference.read, so a recipe's variable uses resolve to the assignments
;                        above. `MAKE` is excluded to keep it a call site and not also a read.
;   include            → @reference.import, named by each included filename.
;   .PHONY and friends → NEITHER a definition nor a call. A special target matching `^\.[A-Z_]+$`
;                        is a directive wearing rule syntax: `.PHONY` defines nothing, and treating
;                        its prerequisites as calls would give every phony target a false inbound
;                        edge from a rule that invokes nothing. The predicate is deliberately
;                        anchored and upper-case-only so a real dotted file target such as
;                        `.envrc` keeps both its symbol and its edges.
;   automatic vars     → `$@`, `$<` and friends are not captured. They name the rule they sit in,
;                        not another symbol.

; ---- rule targets ----
(
  (rule
    (targets
      (word) @name)) @definition.function
  (#not-match? @name "^\\.[A-Z_]+$"))

; ---- variable assignments, in both the expansion and the shell-output form ----
(variable_assignment
  name: (word) @name) @definition.constant

(shell_assignment
  name: (word) @name) @definition.constant

; ---- define ... endef: make's user-defined function ----
(define_directive
  name: (word) @name) @definition.function

; ---- prerequisites are the make call graph; special targets are excluded ----
(
  (rule
    (targets
      (word) @_target)
    (prerequisites
      (word) @name @reference.call))
  (#not-match? @_target "^\\.[A-Z_]+$"))

; ---- $(MAKE): recursive make ----
(
  (variable_reference
    (word) @name) @reference.call
  (#eq? @name "MAKE"))

; ---- $(call fn,args): the leading `.` anchors the name to the FIRST argument ----
(function_call
  function: "call"
  (arguments
    . (text) @name)) @reference.call

; ---- every other $(VAR) read ----
(
  (variable_reference
    (word) @name) @reference.read
  (#not-eq? @name "MAKE"))

; ---- include directives ----
(include_directive
  filenames: (list
    (word) @name)) @reference.import
