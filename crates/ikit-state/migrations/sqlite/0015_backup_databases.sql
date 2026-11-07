-- Each backup is one database of its service. NULL for backups taken before this, which hold
-- the service's default database (POSTGRES_DB or MYSQL_DATABASE).
ALTER TABLE backups ADD COLUMN database TEXT;
