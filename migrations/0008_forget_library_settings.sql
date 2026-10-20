-- Media library folders are set only with environment variables at launch;
-- forget any saved through the settings page by an earlier build.
DELETE FROM settings WHERE key IN ('library.movies_dir', 'library.tv_dir', 'library.unsorted_dir');
