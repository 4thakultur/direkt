-- year: release year found by the metadata lookup on the confirmation step.
-- post_target: where media organization is moving the file. Recorded before
-- the move, so a move interrupted by a restart can be recognized.
ALTER TABLE downloads ADD COLUMN year INTEGER;
ALTER TABLE downloads ADD COLUMN post_target TEXT;
