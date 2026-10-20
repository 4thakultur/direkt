-- Validators remembered from the last response, used with If-Range so a
-- partial file is only resumed if the remote file has not changed.
ALTER TABLE downloads ADD COLUMN resume_etag TEXT;
ALTER TABLE downloads ADD COLUMN resume_last_modified TEXT;
-- Final location of the completed file. Recorded before the partial file is
-- moved, so an interrupted move can be recognized on restart.
ALTER TABLE downloads ADD COLUMN file_path TEXT;
