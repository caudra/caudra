; Workcell SQL tags — written against tree-sitter-sequel 0.3.11 (`grammar()` returns
; `tree_sitter_sequel::LANGUAGE`), verified against an AST dump of tests/fixtures/sample.sql with
; `cargo run -p workcell-source-languages --example dump`.
;
; SQL is a CODE lane (`Language::Sql.family() == LanguageFamily::Code`), so it owes the graph call
; edges. It has two kinds of them and both are captured:
;   - `invocation` — `count(x)`, `account_balance(1)`, and a trigger's `EXECUTE FUNCTION f()`. These
;     are literal calls.
;   - `relation` — the table or view named in `FROM` / `JOIN` / `UPDATE` / `INSERT INTO`. This is
;     SQL's real dependency edge: a view or function that reads a table depends on it exactly the
;     way a function depends on a callee. Modelling it as a `@reference.read` instead would leave a
;     schema-only or migration-only file with an edgeless graph, which ranks every table
;     identically and still looks like a working map. `CREATE TABLE`/`VIEW` are `@definition.class`
;     and `SymbolKind::Class.callable()` is true, so the edge lands on a real target.
;
; ── WHAT THE GRAMMAR CANNOT REACH (probed, not assumed) ──────────────────────────────────────
;   - `CREATE PROCEDURE` is UNIMPLEMENTED upstream — grammar.js carries a literal `// TODO:
;     procedure` and the statement parses to an ERROR node whose dollar-quoted body then leaks out
;     as sibling top-level statements. There is no node kind to write a pattern against, so
;     procedures contribute no definition. `CREATE FUNCTION` covers the routine lane.
;   - Dynamic SQL — `EXECUTE format(..)`, string-built statements, and anything inside a client
;     driver — is a string literal to this grammar. No parser recovers those call edges.
;   - Dialect bodies (PL/pgSQL, T-SQL) inside `$$ .. $$` parse as ordinary SQL statements when they
;     happen to be SQL and as noise when they are not; `BEGIN/DECLARE/LOOP` blocks are not modelled.
;   - Schema and database qualifiers are DROPPED: `billing.account` is the symbol `account`, taken
;     from the `name:` field of `object_reference`. Capturing the qualifier as `@qualifier` would
;     need a second, qualified copy of every pattern, and the qualified copy would double-count
;     every reference the unqualified one already matched.
;   - CTE names (`WITH x AS ..`), aliases, and bare column reads in `SELECT`/`WHERE`/`GROUP BY` are
;     not captured. They are the highest-frequency nodes in any real query and would swamp the map
;     with edge-free nodes, the same posture the bash query takes towards shell variables.
;
; Definition captures sit on the `create_*` / `column_definition` node that owns the statement,
; never on a `column_definitions` or `select_expression` list wrapper.

; ---- definitions: namespaces ----

(create_schema (identifier) @name) @definition.module

; ---- definitions: relations and other named objects ----
;
; Each `create_*` below has exactly one direct `object_reference`; the ones in `REFERENCES`
; constraints and in the view body sit deeper and are not direct children, so no anchor is needed.

(create_table (object_reference name: (identifier) @name)) @definition.class

(create_view (object_reference name: (identifier) @name)) @definition.class

(create_materialized_view (object_reference name: (identifier) @name)) @definition.class

(create_type (object_reference name: (identifier) @name)) @definition.class

; `CREATE INDEX name ON tbl (..)` — the index name is the grammar's `column:` field, oddly named
; upstream but unambiguous: `index_fields` holds the indexed columns under `field` nodes instead.
(create_index column: (identifier) @name) @definition.class

; `CREATE SEQUENCE` needs the anchors: an `OWNED BY tbl.col` tail carries a SECOND direct
; `object_reference` that an unanchored pattern would mint as a duplicate definition. The two
; patterns are exhaustive and disjoint — the name follows either `SEQUENCE` or `IF NOT EXISTS`.
(create_sequence (keyword_sequence) . (object_reference name: (identifier) @name)) @definition.class

(create_sequence (keyword_exists) . (object_reference name: (identifier) @name)) @definition.class

; ---- definitions: routines ----

; Anchored past `keyword_function` so a `RETURNS <user type>` clause cannot supply the name.
; `OR REPLACE` sits before the keyword and so does not disturb the anchor.
(create_function
  (keyword_function) . (object_reference name: (identifier) @name)) @definition.function

; A trigger holds up to three `object_reference` children: its own name, the table after `ON`, and
; the routine after `EXECUTE`. Only the trigger's own name lies between `TRIGGER` and `ON`, and
; ordering alone separates it — which keeps working when `IF NOT EXISTS` is present.
(create_trigger
  (keyword_trigger)
  (object_reference name: (identifier) @name)
  (keyword_on)) @definition.function

; ---- definitions: columns ----
;
; Covers `CREATE TABLE`, `ALTER TABLE .. ADD COLUMN`, and `CREATE TYPE .. AS (..)` alike: all three
; reach the same `column_definition` node.
(column_definition name: (identifier) @name) @definition.field

; ---- references: calls ----

; `count(x)`, `account_balance(1)`. Anchored to the first child because the `EXTRACT(unit FROM x)`
; form carries a second `object_reference` in its `unit:` field.
(invocation . (object_reference name: (identifier) @name)) @reference.call

; `FROM tbl`, `JOIN tbl`, `UPDATE tbl`, `INSERT INTO tbl`. Anchored so a trailing alias cannot
; supply the name; a `relation` wrapping a subquery or an invocation simply does not match here.
(relation . (object_reference name: (identifier) @name)) @reference.call

; `EXECUTE FUNCTION f()` / `EXECUTE PROCEDURE f()` in a trigger body. The routine is the only
; `object_reference` following `EXECUTE`. The capture sits on the reference itself, not on the
; enclosing `create_trigger`, so the use site reports the routine's own line.
(create_trigger
  (keyword_execute)
  (object_reference name: (identifier) @name) @reference.call)

; ---- references: structural dependencies that are not calls ----
;
; A foreign key, an index, a trigger's target table, and a migration all depend on an object without
; invoking anything. These are the use sites that make a schema-only file findable from the table it
; touches, and they stay `@reference.read` because no execution flows through them.

; `account_id BIGINT REFERENCES billing.account (account_id)` — the foreign-key target. Keyed on
; `keyword_references` so a user-defined column TYPE, which also lowers to an `object_reference`
; here, is not mistaken for a foreign key.
(column_definition
  (keyword_references)
  (object_reference name: (identifier) @name) @reference.read)

; `CREATE INDEX .. ON tbl` — the indexed table is the node's only direct `object_reference`, since
; the index's own name is an `identifier` in the `column:` field.
(create_index (object_reference name: (identifier) @name) @reference.read)

; `CREATE TRIGGER .. ON tbl` — anchored, because the routine named after `EXECUTE` is also a sibling
; following `ON` and an unanchored pattern would tag that call site as a read as well.
(create_trigger
  (keyword_on) . (object_reference name: (identifier) @name) @reference.read)

(alter_table (object_reference name: (identifier) @name) @reference.read)

[
  (drop_table (object_reference name: (identifier) @name) @reference.read)
  (drop_view (object_reference name: (identifier) @name) @reference.read)
  (drop_type (object_reference name: (identifier) @name) @reference.read)
  (drop_function (object_reference name: (identifier) @name) @reference.read)
]
