-- Extras of a movie (kind 5, under it by parent_id): DVD extra titles,
-- files of Extras/Trailers/… folders, `-trailer` files. Their kind of extra:
-- trailer, featurette, behind_the_scenes, deleted_scene, interview, scene,
-- short or extra.
ALTER TABLE items ADD COLUMN extra_type VARCHAR(32) NULL;
