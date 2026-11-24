-- The tick lease, for engines run with their background loops off
-- (JAMJET_BACKGROUND=off and POST /ops/tick). One row, name = 'tick', made by
-- the first tick, shared by every engine on the database. A tick takes the
-- lease with a conditional write on the database clock and gives it back
-- with one that names its holder, so two ticks never hold it at once and a
-- tick that lost it changes nothing. cursor is where the last finished tick
-- stopped its walk; last_report is its report as JSON text. The columns are
-- SQLite's, whose table the store creates at open, outside its migrations.

CREATE TABLE engine_ticks (name text PRIMARY KEY, holder text, lease_until timestamptz,
  last_started_at timestamptz, last_finished_at timestamptz, last_complete boolean,
  cursor text, last_report text);
