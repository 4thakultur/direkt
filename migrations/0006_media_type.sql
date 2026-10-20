-- Chosen on the confirmation step before a download starts.
-- media_type: movie, tv, game, music, software, unsorted (NULL: not chosen).
-- title: title found by the metadata lookup.
-- artwork_id: cached image in <data_dir>/artwork.
ALTER TABLE downloads ADD COLUMN media_type TEXT;
ALTER TABLE downloads ADD COLUMN title TEXT;
ALTER TABLE downloads ADD COLUMN artwork_id TEXT;
