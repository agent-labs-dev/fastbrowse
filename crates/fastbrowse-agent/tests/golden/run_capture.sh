#!/usr/bin/env bash
# Records retrieval.json and verification.json from the Python tests, then the long-page cases the size cap leaves
# out (the relevance pass, multi-chunk reads, long answers) into retrieval_large.json and verification_large.json.
#
#     crates/fastbrowse-agent/tests/golden/run_capture.sh          # both
#     crates/fastbrowse-agent/tests/golden/run_capture.sh large    # the long-page cases only
set -euo pipefail
cd "$(dirname "$0")/../../../.."
export PYTHONPATH=crates/fastbrowse-agent/tests/golden

if [[ "${1:-all}" != large ]]; then
uv run pytest -q -p golden_capture \
    tests/test_retrieval.py tests/test_done_check.py tests/test_aggregate_answers.py tests/test_answer_notes_budget.py \
    tests/test_answer_repair.py tests/test_collection_absence.py tests/test_collection_coverage.py \
    tests/test_finish_fields.py tests/test_finish_overlap.py tests/test_finish_partial_fields.py \
    tests/test_output_schema.py tests/test_partitioned_finish.py tests/test_read_before_leaving.py \
    tests/test_record_filter.py tests/test_recovery_sections.py tests/test_redrawn_read_recovery.py \
    tests/test_sign_in_wall_stop.py tests/test_memory.py tests/test_agent.py
fi

large=(
    test_a_long_page_is_focused_and_its_short_fact_read_by_jev
    test_absent_after_focus_still_reaches_the_reader
    test_a_failed_focus_pass_falls_back_to_the_reader
    test_relevant_passages_too_many_to_offer_go_to_the_reader
    test_a_long_block_is_judged_on_all_of_its_text
    test_a_focus_pass_the_spend_limit_cannot_cover_sends_nothing
    test_extract_selects_across_a_candidate_pool_over_jevs_option_cap
    test_extract_group_selection_reserves_each_jev_call
    test_a_later_chunk_saying_the_list_goes_on_reopens_an_earlier_chunks_claim
    test_only_the_last_chunk_names_the_control_that_shows_the_rest
    test_a_list_that_runs_on_into_the_next_chunk_is_settled_by_the_last_one
    test_a_later_chunk_is_read_against_what_earlier_chunks_of_the_page_found
    test_tally_claim_checks_judge_counted_records_by_their_tally
    test_a_ranking_over_every_counted_record_leaves_the_omission_check_its_notes
    test_tallied_quotes_fit_agent_render_paths
    test_comparison_fields_refuse_ambiguous_delimiters
    test_novelty_is_omitted_when_comparing_quotes_exceeds_input_budget
    test_reader_collects_next_project_when_earlier_requirement_evidence_does_not_fit
    test_duplicate_prior_packets_do_not_displace_the_novelty_check
    test_rejected_order_dense_records_use_smaller_response_chunks
    test_short_fact_choice_preserves_equal_captions_control_positions
    test_a_quote_crossing_a_dom_observation_is_not_literal_page_text
)
ids=()
for name in "${large[@]}"; do
    ids+=("tests/test_retrieval.py::$name")
done
GOLDEN_SUFFIX=_large GOLDEN_PER_TEST=1 GOLDEN_MAX_CASE_BYTES=3000000 uv run pytest -q -p golden_capture "${ids[@]}"
