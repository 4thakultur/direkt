-- Password for provider files that are password protected (e.g. 1fichier).
-- Never returned by the API or logged.
ALTER TABLE downloads ADD COLUMN download_password TEXT;
