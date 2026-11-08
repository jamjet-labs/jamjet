-- The lease fence under which the claim gate last released an item's
-- payload (POST /work-items/claim with execution_id). A claim that hands the
-- same lease out again re-checks it as a completion would only if this
-- matches; any other lease is decided again as a new claim.
ALTER TABLE work_items ADD COLUMN released_fence INTEGER;
