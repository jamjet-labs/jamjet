//! One behaviour suite for every [`StateBackend`].
//!
//! A case is an async function in [`cases`] that takes a [`Fixture`]. A
//! [`StoreFactory`] makes a fresh fixture for one backend, and
//! [`conformance_suite!`](crate::conformance_suite) turns every case into a
//! `#[tokio::test]` on that factory, so a backend's suite is one line in
//! `testkit/tests/`:
//!
//! ```text
//! jamjet_testkit::conformance_suite!(jamjet_testkit::conformance::SqliteFactory);
//! ```
//!
//! A case reaches the store only through the fixture: the trait on
//! [`Fixture::backend`], [`Fixture::second`] and [`Fixture::scoped`], and
//! [`StoreHooks`] for what the trait cannot do (backdate a lease, read a raw
//! row, reopen the store). So the same case runs unchanged on every backend.

pub mod cases;
mod factories;
mod hooks;

pub use factories::{MemoryFactory, SqliteFactory, SqliteScopedFactory};
pub use hooks::{ItemRow, StoreHooks, StoreRows, StoredJson};

use async_trait::async_trait;
use jamjet_state::{StateBackend, TenantId};
use std::sync::Arc;

/// The store seen through one tenant, as the server's per-request backend
/// sees it.
pub type Scoper = Arc<dyn Fn(&TenantId) -> Arc<dyn StateBackend> + Send + Sync>;

/// Which backend a fixture holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Memory,
    Sqlite,
    /// SQLite through `TenantScopedSqliteBackend`.
    SqliteScoped,
    Postgres,
    /// Postgres scoped to a tenant.
    PostgresScoped,
}

impl StoreKind {
    /// Whether [`Fixture::backend`] is a tenant-scoped view.
    pub fn is_scoped(self) -> bool {
        matches!(self, StoreKind::SqliteScoped | StoreKind::PostgresScoped)
    }
}

/// A fresh store for one case. Fields drop in order, so `keep` (the temp
/// directory or the test database) goes last, after every pool on it.
pub struct Fixture {
    /// The backend the case runs against. Unscoped for `Memory`, `Sqlite`
    /// and `Postgres`, as the engine holds it. For the scoped kinds, the
    /// default tenant's scoped view, so every case runs through the scoped
    /// code.
    pub backend: Arc<dyn StateBackend>,
    /// The same store through a pool of its own, as a second engine sees
    /// it, and scoped like `backend`. On memory, the same `Arc`: one
    /// process, so a race between the two handles races the backend's own
    /// locking.
    pub second: Option<Arc<dyn StateBackend>>,
    /// The same store, and `backend`'s pool, seen through any tenant. Set
    /// for the scoped kinds only.
    pub scoped: Option<Scoper>,
    /// What a case needs and the trait does not offer.
    pub hooks: Arc<dyn StoreHooks>,
    pub kind: StoreKind,
    /// What keeps the store alive: a temp directory, a test database, or
    /// nothing.
    pub keep: Box<dyn std::any::Any + Send + Sync>,
}

/// Makes a fresh [`Fixture`] per case.
#[async_trait]
pub trait StoreFactory: Send + Sync {
    /// `None` means the store is not configured here and the case skips:
    /// Postgres with no `JAMJET_TEST_POSTGRES_URL` and no
    /// `JAMJET_REQUIRE_POSTGRES_TESTS`.
    async fn fresh(&self) -> Option<Fixture>;
}

/// Expands every case in [`cases`] into a test on `$factory`, one module
/// per case file.
#[macro_export]
macro_rules! conformance_suite {
    ($factory:expr) => {
        $crate::for_each_case!($crate::__conformance_module, $factory);
    };
}

/// Calls `$m!($factory, <module>, [<case>, ...]);` once per case module.
/// A case must be listed here to run.
#[macro_export]
macro_rules! for_each_case {
    ($m:path, $factory:expr) => {
        $m!(
            $factory,
            fixture,
            [
                the_second_handle_sees_what_the_first_wrote,
                item_row_follows_a_claim_and_a_park,
                a_forced_expiry_is_reclaimed_and_a_future_one_is_not,
                the_store_term_only_rises_and_reaches_the_fence,
                a_dead_lettered_item_names_its_run,
                artifact_rows_count_one_copy_per_tenant,
                a_token_moved_to_another_tenant_validates_there,
                a_reopened_store_keeps_what_was_written,
                the_row_hooks_answer_wherever_the_store_keeps_rows,
                a_scoped_handle_sees_only_its_tenant,
            ]
        );
        $m!(
            $factory,
            api_token_hash,
            [
                a_token_stored_by_an_earlier_build_still_validates,
                a_new_token_stores_the_hex_sha256_of_its_text,
            ]
        );
        $m!(
            $factory,
            append_event_if,
            [
                an_append_lands_only_while_its_condition_holds,
                a_terminal_or_unknown_execution_is_refused,
                the_condition_sees_writes_made_after_the_callers_read,
                execution_tenant_is_the_rows_tenant_and_is_scoped,
                log_order_matches_sequence_order_under_concurrency,
            ]
        );
        $m!(
            $factory,
            event_compat,
            [an_event_kind_from_a_later_release_reads_as_unknown]
        );
        $m!(
            $factory,
            events_page,
            [a_page_stops_at_its_budget_on_every_backend]
        );
        $m!(
            $factory,
            initiator,
            [
                segment_inherits_the_initiator,
                a_segment_of_a_dev_mode_run_has_no_initiator,
            ]
        );
        $m!(
            $factory,
            running_page,
            [
                pages_every_running_run,
                an_unreadable_row_does_not_hide_the_rows_after_it,
            ]
        );
        $m!(
            $factory,
            workflow_immutability,
            [the_backend_refuses_to_replace_a_version]
        );
        $m!(
            $factory,
            durability,
            [
                test_state_machine_transitions,
                test_crash_recovery_from_event_log,
                test_recovery_with_snapshot,
                test_event_sequencing,
            ]
        );
        $m!(
            $factory,
            lease_semantics,
            [
                a_claim_names_its_lease_length_and_renewals_keep_it,
                the_fast_path_reclaim_records_the_retry_in_the_history,
                a_heartbeat_reattaches_a_lease_lost_to_expiry_when_nobody_else_claimed,
                a_heartbeat_does_not_reattach_once_another_worker_holds_the_item,
                a_heartbeat_does_not_reattach_after_another_worker_claimed_and_released,
                a_heartbeat_with_the_wrong_fence_does_not_reattach,
                a_heartbeat_from_another_worker_does_not_reattach,
                a_claimed_lease_is_bounded_by_the_backend,
                a_commit_into_a_terminal_run_records_nothing_and_honours_the_fence,
                a_lease_lapsing_on_a_terminal_run_records_no_retry_and_settles_the_item,
                finishing_a_run_writes_status_and_event_together_and_only_once,
            ]
        );
        $m!(
            $factory,
            scheduler_pass,
            [
                a_pass_lands_events_items_and_finish_together,
                a_stale_pass_writes_nothing,
                a_pass_for_a_node_with_a_live_item_is_stale,
                a_pass_on_a_finished_run_is_refused,
                a_pass_item_of_another_run_is_refused,
                a_pass_that_does_not_hold_together_is_refused,
                a_pass_stamps_items_with_the_runs_tenant,
                a_scoped_pass_for_a_run_of_another_tenant_is_not_found,
                two_concurrent_passes_at_one_sequence_commit_once,
                a_pass_and_a_worker_commit_racing_land_one_after_the_other,
            ]
        );
        $m!(
            $factory,
            lease_events,
            [
                reclaim_events_match_the_old_trail,
                reclaim_on_a_finished_run_records_nothing,
                a_recorded_failure_writes_the_fail_routes_trail,
                a_recorded_failure_on_a_finished_run_records_nothing,
                a_recorded_failure_with_a_stale_fence_writes_nothing,
                a_scoped_store_reclaims_and_fails_only_its_own_tenants_items,
            ]
        );
        $m!(
            $factory,
            memory_backend,
            [
                test_workflow_crud,
                test_execution_lifecycle,
                test_list_executions_with_filter,
                test_event_log,
                test_snapshots,
                test_work_queue,
                test_complete_work_item_fenced,
                unfenced_complete_settles_only_a_claimed_item,
                fenced_complete_rejects_stale_fence_after_reclaim,
                fenced_complete_rejects_forged_fence_zero_on_pending_item,
                fenced_complete_settles_claimed_item_with_matching_fence,
                fenced_complete_is_atomic_under_concurrency,
                test_patch_append_array,
                test_api_tokens,
            ]
        );
        $m!(
            $factory,
            projector,
            [
                apply_and_read,
                upsert_idempotency,
                unknown_checkpoint_is_zero,
                two_nodes_same_execution,
                tenant_isolation,
            ]
        );
        $m!(
            $factory,
            park,
            [
                park_correct_fence_returns_true_and_resets_row,
                park_stale_fence_returns_false_row_unchanged,
                park_wrong_fence_on_claimed_item_is_noop,
                park_fence_guard_and_epoch_bump,
                park_past_retry_after_is_immediately_claimable,
                fence_safety_claim_park_reclaim_stale_park,
                attempt_monotonicity_and_epoch_freshness,
            ]
        );
        $m!(
            $factory,
            fencing,
            [
                claim_mints_nonzero_fence,
                reclaim_bumps_fence,
                commit_succeeds_with_correct_fence,
                commit_fails_closed_with_stale_fence,
                fence_survives_lost_tail_failover,
                term_pin_reopens_window_negative_control,
                crash_before_commit_then_reclaim_yields_exactly_one_terminal,
                commit_node_terminal_node_failed_settles_failed,
                commit_node_terminal_node_failed_stale_fence_fails_closed,
                commit_node_failed_stale_fence_fails_closed,
                set_store_term_at_least_is_monotonic_and_lifts_the_fence,
                claim_side_lease_expiry_consumes_attempts,
                reclaim_cannot_resurrect_an_item_settled_under_it,
                a_failed_worker_cannot_park_its_requeued_item,
            ]
        );
        $m!(
            $factory,
            idempotency,
            [
                tool_effect_round_trip,
                a_recorded_effect_keeps_its_cost,
                no_key_no_effect_recorded,
                concurrent_winner_insert_or_ignore,
                tool_effect_survives_reopen,
                tenant_scoped_tool_effect_round_trip,
                only_one_worker_can_claim_an_idempotency_key,
                a_reservation_is_renewed_only_by_the_attempt_that_holds_it,
                an_expired_reservation_can_be_taken_over,
                distinct_keys_do_not_contend,
                exactly_one_of_many_concurrent_claimants_wins,
                releasing_a_reservation_frees_it_immediately,
                a_non_holder_cannot_release_someone_elses_reservation,
                count_node_completions_matches_the_scan,
            ]
        );
        $m!(
            $factory,
            commit_turn,
            [
                test_snapshot_full_materialized_state_round_trips,
                test_materialize_equals_full_apply_after_snapshot,
                commit_turn_with_snapshot_writes_event_and_snapshot,
                commit_turn_stale_fence_writes_nothing,
                fresh_process_resume,
                cross_backend_resume,
                concurrent_sibling_commits_no_write_skew,
                concurrent_sibling_commits_race,
            ]
        );
        $m!(
            $factory,
            segment,
            [
                linked_execution_round_trips,
                root_execution_has_defaults,
                segment_boundary_event_is_materialize_noop,
                start_next_segment_creates_a_linked_seeded_continuation,
                segment_chain_lineage,
                bounded_replay_independent_of_root_event_count,
                inert_old_segment_does_not_leak,
                two_rollover_byte_identity,
                child_forward_progress_after_rollover,
                idempotent_start_next_segment_prevents_double_roll,
                all_artifacts_present_after_start_next_segment,
                segment_atomic_rollback_on_work_item_conflict,
            ]
        );
        $m!(
            $factory,
            artifacts,
            [
                put_returns_stable_hash_and_size,
                get_returns_original_bytes,
                put_same_bytes_twice_dedupes,
                get_missing_hash_returns_none,
                tenant_scoped_isolation_a_cannot_read_b_artifact,
                tenant_scoped_identical_content_stored_twice,
                tenant_scoped_put_dedupes_within_same_tenant,
                resolve_value_non_conforming_artifact_key_returns_unchanged,
                resolve_value_round_trip,
                resolve_value_non_sentinel_passes_through,
                resolve_value_missing_artifact_returns_error,
                resolve_value_walks_object_one_level,
                round_trip_string_value_above_threshold,
                round_trip_deeply_nested_object_above_threshold,
                round_trip_array_of_objects_above_threshold,
                round_trip_unicode_and_special_chars_above_threshold,
                dedupe_at_scale_fifty_kb_blob_put_ten_times,
                dedupe_two_distinct_blobs_produce_two_rows_and_different_hashes,
                tenant_cross_resolve_value_returns_error_no_cross_tenant_read,
                tenant_scoped_full_spill_resolve_round_trip,
                tenant_scoped_dangling_ref_returns_error_no_panic,
                deduped_put_returns_stored_media_type,
                deduped_put_returns_stored_null_media_type,
            ]
        );
        $m!(
            $factory,
            tenant_isolation,
            [
                create_and_list_tenants,
                get_and_update_tenant,
                workflow_definitions_are_tenant_isolated,
                executions_are_tenant_isolated,
                events_are_tenant_isolated,
                work_items_are_tenant_isolated,
                default_tenant_backward_compatible,
                tokens_carry_tenant_id,
                scoped_dead_letter_stores_the_bare_execution_uuid,
                scoped_fail_cannot_reach_another_tenants_item,
                scoped_unfenced_complete_cannot_reach_another_tenants_item,
                scoped_reclaim_cannot_resurrect_a_settled_item,
                scoped_claim_side_expiry_consumes_attempts,
                a_reservation_is_scoped_to_its_tenant,
                get_work_item_is_scoped_to_the_tenant,
                a_token_carries_the_tenant_on_its_row,
            ]
        );
        $m!(
            $factory,
            concurrency,
            [
                two_handles_claim_one_item_once,
                late_completion_after_reclaim_is_refused_by_fence,
                late_heartbeat_after_another_claim_cannot_reattach,
                two_finishes_record_one_terminal_event,
                decision_and_expiry_of_one_request_land_once,
                concurrent_appends_keep_sequences_dense,
                concurrent_array_appends_keep_every_value,
                #[ignore = "known gap: a stale projection batch can move a row back; fixed with the Postgres audit change"]
                a_stale_projection_batch_never_regresses,
            ]
        );
        $m!(
            $factory,
            payloads,
            [
                nul_in_json_round_trips_byte_equal,
                event_json_reads_back_byte_equal,
                a_16_mib_event_round_trips,
                #[ignore = "finding: SQLite page budget counts characters"]
                events_page_keeps_its_byte_budget_with_multibyte_text,
            ]
        );
        $m!(
            $factory,
            terminal,
            [
                plain_append_on_a_finished_run_changes_nothing,
                commit_turn_on_a_finished_run_withdraws,
            ]
        );
        $m!(
            $factory,
            tenancy,
            [
                scoped_claim_never_takes_another_tenants_item,
                scoped_claim_for_execution_of_another_tenant_is_nothing,
                scoped_fast_reclaim_touches_its_own_tenant_only,
                scoped_append_to_another_tenants_run_is_not_found,
                scoped_commit_into_another_tenants_run_is_not_found,
                scoped_snapshot_of_another_tenants_run_is_not_found,
                scoped_enqueue_into_another_tenants_run_is_not_found,
                scoped_projection_of_another_tenants_run_is_not_found,
                scoped_start_naming_another_tenants_run_is_not_found,
                scoped_checked_writes_to_another_tenants_run_are_not_found,
                scoped_reads_of_another_tenants_rows_are_empty,
            ]
        );
        $m!(
            $factory,
            run_keys,
            [
                a_key_started_twice_makes_one_run,
                two_starts_with_one_key_at_once_make_one_run,
                scoped_lookup_of_another_tenants_key_finds_nothing,
                scoped_keyed_start_naming_another_tenants_run_is_not_found,
            ]
        );
        $m!(
            $factory,
            faults,
            [
                an_error_at_any_step_leaves_no_partial_write,
                a_hang_inside_a_transaction_blocks_only_its_run,
            ]
        );
    };
}

/// One module of tests, one per case, each on a fresh fixture. Attributes
/// written before a case in the list (`#[ignore = "..."]`) go on its test.
#[doc(hidden)]
#[macro_export]
macro_rules! __conformance_module {
    ($factory:expr, $module:ident, [$($(#[$meta:meta])* $case:ident),* $(,)?]) => {
        mod $module {
            #[allow(unused_imports)]
            use super::*;
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$meta])*
                async fn $case() {
                    let Some(fx) = $crate::conformance::StoreFactory::fresh(&$factory).await else {
                        return $crate::skip(stringify!($case));
                    };
                    $crate::conformance::cases::$module::$case(&fx).await;
                }
            )*
        }
    };
}
