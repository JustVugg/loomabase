-- Run after Loomabase schema migration. This creates a group role; create a
-- separate LOGIN role with a secret password and grant this role to it.
DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'loomabase_runtime') THEN
        CREATE ROLE loomabase_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE INHERIT;
    END IF;
END
$$;

GRANT USAGE ON SCHEMA public TO loomabase_runtime;
GRANT SELECT ON loomabase_server_state TO loomabase_runtime;
GRANT SELECT, INSERT, UPDATE, DELETE
    ON loomabase_state, loomabase_cursor_lease, loomabase_audit_log, todos, todos_crdt
    TO loomabase_runtime;
GRANT USAGE, SELECT ON SEQUENCE loomabase_seq TO loomabase_runtime;

-- For custom contracts, grant DML on each generated application and CRDT table.
-- Example:
-- GRANT SELECT, INSERT, UPDATE, DELETE ON notes, notes_crdt TO loomabase_runtime;
