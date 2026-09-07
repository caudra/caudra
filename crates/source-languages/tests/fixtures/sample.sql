-- Fixture for the SQL tags query (tree-sitter-sequel).

CREATE SCHEMA billing;

CREATE TYPE billing.invoice_state AS ENUM ('draft', 'sent', 'paid');

CREATE SEQUENCE billing.invoice_seq START WITH 1;

CREATE SEQUENCE IF NOT EXISTS billing.account_seq INCREMENT BY 2;

CREATE TABLE billing.account (
    account_id BIGINT PRIMARY KEY,
    display_name TEXT NOT NULL,
    balance_cents BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE billing.invoice (
    invoice_id BIGINT PRIMARY KEY,
    account_id BIGINT NOT NULL REFERENCES billing.account (account_id),
    total_cents BIGINT NOT NULL
);

CREATE INDEX invoice_account_idx ON billing.invoice (account_id);

CREATE VIEW overdue_invoice AS
SELECT invoice_id, total_cents
FROM billing.invoice
JOIN billing.account ON account.account_id = invoice.account_id
WHERE total_cents > 0;

CREATE MATERIALIZED VIEW billing.account_rollup AS
SELECT account_id FROM billing.invoice;

CREATE FUNCTION billing.account_balance(target_id BIGINT) RETURNS BIGINT AS $$
    SELECT balance_cents FROM billing.account WHERE account_id = target_id;
$$ LANGUAGE SQL;

CREATE OR REPLACE FUNCTION billing.audit_invoice() RETURNS TRIGGER AS $$
    SELECT account_balance(1);
$$ LANGUAGE SQL;

CREATE TRIGGER invoice_audit
AFTER INSERT ON billing.invoice
FOR EACH ROW
EXECUTE FUNCTION billing.audit_invoice();

SELECT count(invoice_id), sum(total_cents), account_balance(1)
FROM billing.invoice
JOIN billing.account ON account.account_id = invoice.account_id
GROUP BY account_id;

UPDATE billing.account SET balance_cents = 0 WHERE account_id = 1;

ALTER TABLE billing.invoice ADD COLUMN issued_at TIMESTAMP;

DROP VIEW overdue_invoice;
