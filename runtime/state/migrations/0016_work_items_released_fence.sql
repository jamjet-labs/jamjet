-- The claim gate's mark on an item under its current lease
-- (POST /work-items/claim with execution_id): the lease fence when the gate
-- released the payload under it, its negation while a gate decides (fences
-- are positive), NULL otherwise. A claim that hands the same lease out again
-- re-checks it as a completion would only if released, waits while a gate
-- runs, and otherwise decides it again as a new claim.
ALTER TABLE work_items ADD COLUMN released_fence INTEGER;
