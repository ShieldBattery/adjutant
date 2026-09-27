-- Run as a database administrator after the base roles/views in docs/database-mcp.md.
-- This is operational provisioning, not a ShieldBattery application migration.
-- Requires ShieldBattery's user-ban unban columns (20260913120000 migration),
-- queue-time rating (20260625120000), and failed formations (20260628120000).
-- Reapplying this file is safe. Any error rolls back the entire update.
BEGIN;

GRANT SELECT (id, user_id, start_time, end_time, banned_by, unbanned_by, unbanned_at)
  ON public.user_bans TO adjutant_view_owner;
GRANT SELECT (id, user_id, kind, start_time, end_time, restricted_by, reason)
  ON public.user_restrictions TO adjutant_view_owner;
GRANT SELECT (user_id, identifier_type, identifier_hash, first_used, last_used, times_seen)
  ON public.user_identifiers TO adjutant_view_owner;
GRANT SELECT (identifier_type, identifier_hash, time_banned, banned_until, first_user_id)
  ON public.user_identifier_bans TO adjutant_view_owner;
GRANT SELECT (id, identifier_type, identifier_hash, kind, start_time, end_time,
              restricted_by, first_user_id, reason)
  ON public.user_identifier_restrictions TO adjutant_view_owner;
GRANT SELECT (id, identifier_type, identifier_hash, triggered_by, ban_level,
              created_at, expires_at, clears_at, cleared)
  ON public.matchmaking_bans TO adjutant_view_owner;

GRANT SELECT (id, user_id, matchmaking_type, completion_type, search_time_millis,
              completion_time, rating)
  ON public.matchmaking_completions TO adjutant_view_owner;
GRANT SELECT (game_id, matchmaking_type, quality, skill_variance, win_probability,
              team_a_rating, team_b_rating, max_latency, created_at, id, fail_phase)
  ON public.matchmaking_match_formations TO adjutant_view_owner;

SET ROLE adjutant_view_owner;

CREATE OR REPLACE VIEW adjutant_diagnostics.user_bans
  WITH (security_barrier = true)
AS
SELECT id, user_id, start_time, end_time, banned_by, unbanned_by, unbanned_at
FROM public.user_bans;

CREATE OR REPLACE VIEW adjutant_diagnostics.user_restrictions
  WITH (security_barrier = true)
AS
SELECT id, user_id, kind::text AS kind, start_time, end_time, restricted_by,
       CASE
         WHEN kind::text = 'chat' AND reason::text IN (
           'spam', 'harassment', 'hate_speech', 'toxicity', 'disruptive_behavior', 'other'
         ) THEN reason::text
         WHEN kind::text = 'matchmaking' AND reason::text IN (
           'cheating', 'left_game', 'griefing'
         ) THEN reason::text
         WHEN kind::text = 'avatar_upload' AND reason::text IN (
           'inappropriate_content', 'harassment', 'hate_speech', 'other'
         ) THEN reason::text
         ELSE NULL
       END AS reason
FROM public.user_restrictions;

-- Metadata only: one row per account/type, not one row per stored hash.
-- Type 0 is excluded from the application's account-linking evidence.
CREATE OR REPLACE VIEW adjutant_diagnostics.user_identifiers
  WITH (security_barrier = true)
AS
SELECT user_id, identifier_type, count(*) AS identifier_count,
       min(first_used) AS first_used, max(last_used) AS last_used,
       sum(times_seen) AS times_seen
FROM public.user_identifiers
WHERE identifier_type <> 0
GROUP BY user_id, identifier_type;

-- One row per account pair/type. Multiple matching hashes of the same type
-- still count as just ONE matching type. Always scope queries to a user_id.
CREATE OR REPLACE VIEW adjutant_diagnostics.user_identifier_matches
  WITH (security_barrier = true)
AS
SELECT source.user_id, linked.user_id AS linked_user_id, source.identifier_type,
       count(*) AS matching_identifier_count,
       min(source.first_used) AS first_used, max(source.last_used) AS last_used,
       min(linked.first_used) AS linked_first_used, max(linked.last_used) AS linked_last_used
FROM public.user_identifiers AS source
JOIN public.user_identifiers AS linked
  ON linked.identifier_type = source.identifier_type
 AND linked.identifier_hash = source.identifier_hash
 AND linked.user_id <> source.user_id
WHERE source.identifier_type <> 0
GROUP BY source.user_id, linked.user_id, source.identifier_type;

-- Associations are evidence, not a decision that a punishment applies.
-- LEFT JOIN retains punishment rows with no remaining identifier association.
CREATE OR REPLACE VIEW adjutant_diagnostics.user_identifier_bans
  WITH (security_barrier = true)
AS
SELECT identifiers.user_id, bans.identifier_type, bans.time_banned, bans.banned_until,
       bans.first_user_id
FROM public.user_identifier_bans AS bans
LEFT JOIN public.user_identifiers AS identifiers
  ON identifiers.identifier_type = bans.identifier_type
 AND identifiers.identifier_hash = bans.identifier_hash
WHERE bans.identifier_type <> 0;

CREATE OR REPLACE VIEW adjutant_diagnostics.user_identifier_restrictions
  WITH (security_barrier = true)
AS
SELECT restrictions.id, identifiers.user_id, restrictions.identifier_type,
       restrictions.kind::text AS kind, restrictions.start_time, restrictions.end_time,
       restrictions.restricted_by, restrictions.first_user_id,
       CASE
         WHEN restrictions.kind::text = 'chat' AND restrictions.reason::text IN (
           'spam', 'harassment', 'hate_speech', 'toxicity', 'disruptive_behavior', 'other'
         ) THEN restrictions.reason::text
         WHEN restrictions.kind::text = 'matchmaking' AND restrictions.reason::text IN (
           'cheating', 'left_game', 'griefing'
         ) THEN restrictions.reason::text
         WHEN restrictions.kind::text = 'avatar_upload' AND restrictions.reason::text IN (
           'inappropriate_content', 'harassment', 'hate_speech', 'other'
         ) THEN restrictions.reason::text
         ELSE NULL
       END AS reason
FROM public.user_identifier_restrictions AS restrictions
LEFT JOIN public.user_identifiers AS identifiers
  ON identifiers.identifier_type = restrictions.identifier_type
 AND identifiers.identifier_hash = restrictions.identifier_hash
WHERE restrictions.identifier_type <> 0;

CREATE OR REPLACE VIEW adjutant_diagnostics.matchmaking_bans
  WITH (security_barrier = true)
AS
SELECT bans.id, identifiers.user_id, bans.identifier_type, bans.triggered_by,
       bans.ban_level, bans.created_at, bans.expires_at, bans.clears_at, bans.cleared
FROM public.matchmaking_bans AS bans
LEFT JOIN public.user_identifiers AS identifiers
  ON identifiers.identifier_type = bans.identifier_type
 AND identifiers.identifier_hash = bans.identifier_hash
 AND identifiers.identifier_type <> 0;

CREATE OR REPLACE VIEW adjutant_diagnostics.matchmaking_completions
  WITH (security_barrier = true)
AS
SELECT id, user_id, matchmaking_type::text AS matchmaking_type,
       completion_type::text AS completion_type, search_time_millis, completion_time, rating
FROM public.matchmaking_completions;

-- Append columns to the existing view in order to preserve its query contract.
-- Failed formations have no game_id and must not be filtered out.
CREATE OR REPLACE VIEW adjutant_diagnostics.matchmaking_formations
  WITH (security_barrier = true)
AS
SELECT game_id, matchmaking_type::text AS matchmaking_type,
       quality, skill_variance, win_probability,
       team_a_rating, team_b_rating, max_latency, created_at, id, fail_phase
FROM public.matchmaking_match_formations;

RESET ROLE;

GRANT SELECT ON adjutant_diagnostics.user_bans,
                adjutant_diagnostics.user_restrictions,
                adjutant_diagnostics.user_identifiers,
                adjutant_diagnostics.user_identifier_matches,
                adjutant_diagnostics.user_identifier_bans,
                adjutant_diagnostics.user_identifier_restrictions,
                adjutant_diagnostics.matchmaking_bans,
                adjutant_diagnostics.matchmaking_completions,
                adjutant_diagnostics.matchmaking_formations
  TO adjutant_diagnostics;

COMMIT;
