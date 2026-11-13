-- Adds completion claims and background QoS restoration ownership.

ALTER TABLE jobs ADD COLUMN finish_state TEXT;
ALTER TABLE jobs ADD COLUMN finish_exit_code INTEGER;
ALTER TABLE jobs ADD COLUMN finish_max_rss INTEGER;
ALTER TABLE background_processes ADD COLUMN job_id TEXT;
CREATE TABLE background_jobs (job_id TEXT PRIMARY KEY);
