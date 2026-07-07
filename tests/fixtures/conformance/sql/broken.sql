-- Conformance fixture (tui-rework 07).
-- Intentional diagnostic: sql-language-server flags the syntax error — a SELECT
-- with a FROM clause but no columns and no table.
SELECT FROM;
