; Workcell CMake tags — written against tree-sitter-cmake 0.7.5 (`tree_sitter_cmake::LANGUAGE`), used
; for `CMakeLists.txt` and `*.cmake`.
;
; CMake is a CODE lane (LanguageFamily::Code), so it emits call edges. Every statement is a command
; invocation, and a `function` or `macro` defines a command that later statements invoke by name.
;
; ── THE TABLE OF DECISIONS ────────────────────────────────────────────────────────────────────────
;   function, macro    → @definition.function and @definition.macro, captured on the `function_def`
;                        or `macro_def` that owns the body, so the calls inside it are attributed to
;                        it, and named by the first argument of the header command.
;   command            → @reference.call, on every invocation. Builtins such as `set` and
;                        `target_link_libraries` meet no definition and mint no edge, as in Bash.
;   target             → @definition.function for `add_library`, `add_executable`, and
;                        `add_custom_target`, named by the target, mirroring Make's rule targets.
;   set, option        → @definition.constant, named by the variable.
;   ${VAR}             → @reference.read.
;   include,           → @reference.import, named by the first argument: the module, package, or
;   find_package,        directory.
;   add_subdirectory
;   Command names are case-insensitive in CMake, so the command predicates match either case.
;   DISCLOSED LIMITATION: only a literal unquoted argument is a name. `include("x.cmake")` is not an
;   import, and a name built from a variable, as in `add_executable(${name} …)` or
;   `include(${dir}/x.cmake)`, neither defines nor imports anything.
;   Resolution compares exact spellings, so a call spelled `Build_All()` does not meet
;   `function(build_all)`.

; ---- function and macro definitions ----
(
  (function_def
    (function_command
      (argument_list
        . (argument
          (unquoted_argument) @name)))) @definition.function
  (#not-match? @name "[$]"))

(
  (macro_def
    (macro_command
      (argument_list
        . (argument
          (unquoted_argument) @name)))) @definition.macro
  (#not-match? @name "[$]"))

; ---- build targets ----
(
  (normal_command
    (identifier) @_command
    (argument_list
      . (argument
        (unquoted_argument) @name))) @definition.function
  (#match? @_command "^(?i:add_library|add_executable|add_custom_target)$")
  (#not-match? @name "[$]"))

; ---- cache and normal variables ----
(
  (normal_command
    (identifier) @_command
    (argument_list
      . (argument
        (unquoted_argument) @name))) @definition.constant
  (#match? @_command "^(?i:set|option)$")
  (#not-match? @name "[$]"))

; ---- every command invocation ----
(normal_command
  (identifier) @name) @reference.call

; ---- ${VAR} reads ----
(normal_var
  (variable) @name) @reference.read

; ---- modules, packages, and subdirectories ----
(
  (normal_command
    (identifier) @_command
    (argument_list
      . (argument
        (unquoted_argument) @name))) @reference.import
  (#match? @_command "^(?i:include|find_package|add_subdirectory)$")
  (#not-match? @name "[$]"))
