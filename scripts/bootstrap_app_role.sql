-- One-time bootstrap, run as the database owner BEFORE migrations:
--     psql "$DATABASE_URL" -v app_password='<password>' -f scripts/bootstrap_app_role.sql
-- Creates the login role the application connects as. Kept out of sqlx
-- migrations because roles are cluster-wide and the password comes from the
-- environment, not source control.
SET bootstrap.app_password = :'app_password';

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'confluence_app') THEN
        EXECUTE format('CREATE ROLE confluence_app LOGIN PASSWORD %L', current_setting('bootstrap.app_password'));
    END IF;
END
$$;
