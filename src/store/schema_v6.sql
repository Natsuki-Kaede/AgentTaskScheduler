-- Adds attention receipts and repairs legacy result acknowledgements.

ALTER TABLE submissions ADD COLUMN attention_state TEXT;
UPDATE submissions SET quiet = 0
WHERE quiet = 1 AND job_id IN (
    SELECT id FROM jobs
    WHERE state NOT IN ('completed', 'start_failed', 'canceled', 'ended_unknown')
      AND main_exit_code IS NULL
);

ALTER TABLE jobs ADD COLUMN pgid_start TEXT;
ALTER TABLE jobs ADD COLUMN launch_pid INTEGER;
ALTER TABLE jobs ADD COLUMN launch_start TEXT;
CREATE TABLE process_boot (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    boot_id TEXT NOT NULL
);
