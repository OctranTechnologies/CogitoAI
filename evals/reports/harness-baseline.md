# Evaluation baseline: coding-agent-core-v1

- Status: **completed**
- Lane: `harness`
- Provider/model: `mock/mock-eval`
- Commit: `7346a09782825d6a7c50e204ef888960c5531626`
- Evaluated source tree: dirty, fingerprint `1abf141b47c8bcc0`

| Tasks | Success | Tests | Regressions | Median seconds | Tokens | Cost |
|---:|---:|---:|---:|---:|---:|---:|
| 10/10 | 100% | 12/12 | 0 | 14.32 | 56919 | n/a |

- Tokens: context_estimate
- Human approvals: 0; approval requests: 17
- Safety violations: 0; context compactions: 0; repeated failures: 0

## Per-task results

| Task | Category | Result | Tests | Turns | Tools | Regressions |
|---|---|---:|---:|---:|---:|---:|
| small-bug-tax-calculation | small_bug | pass | 1/1 | 5 | 4 | 0 |
| feature-slugify | feature | pass | 2/2 | 4 | 3 | 0 |
| repair-failing-discount-test | failing_test_repair | pass | 1/1 | 5 | 4 | 0 |
| refactor-shared-total-across-files | cross_file_refactor | pass | 1/1 | 6 | 5 | 0 |
| update-greeting-api | api_update | pass | 2/2 | 5 | 4 | 0 |
| trace-request-routing | unfamiliar_code_trace | pass | 0/0 | 5 | 4 | 0 |
| add-port-validation | validation | pass | 2/2 | 5 | 4 | 0 |
| fix-type-annotation | type_error | pass | 1/1 | 5 | 4 | 0 |
| clarify-ambiguous-output-request | ambiguous_requirement | pass | 0/0 | 2 | 1 | 0 |
| preserve-dirty-tree-while-fixing-config | dirty_git_tree | pass | 2/2 | 5 | 4 | 0 |

Harness-lane tokens are context estimates; model-lane tokens are provider-reported when available. Cost is omitted unless explicit rates are supplied. This report does not combine scripted harness validation with live-model quality.
