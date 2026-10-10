-- The principal (`token:<id>`) that created or last replaced a cron job.
--
-- Runs a cron job starts record this principal as their initiator, so the
-- person who chose the workflow, the input and the schedule cannot approve
-- the run (separation of duties). NULL for jobs created before this column
-- existed and for jobs created in dev mode: their runs have no initiator, and
-- an approval on such a run is refused outside dev mode.
ALTER TABLE cron_jobs ADD COLUMN created_by TEXT;
