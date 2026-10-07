# Coding-agent evaluation policy

The evaluation suite is the decision gate for coding-agent changes. A feature
proposal must name its expected measurable benefit, the tasks that exercise it,
and the metrics that must not regress. Record a baseline before implementation,
then run the same suite, fixture set, provider, and model after implementation.

Harness quality and model quality are separate lanes:

- The **harness lane** replays fixed, deterministic tool traces through the real
  CLI, RPC runtime, permission path, session log, verification pipeline, and
  workspace. It measures orchestration and safety behavior; it does not claim to
  measure model reasoning.
- The **model lane** sends the same task prompts to one selected real provider and
  model. It measures end-to-end model-plus-harness task quality. Compare only the
  same provider and model when attributing a change to the harness. Never merge
  harness-lane and model-lane scores into one quality number.

Before considering the core single-agent loop reliable, require a complete
10/10 harness-lane run and at least two credentialed model runs from different
provider families. Each model run must complete all task cases, reach at least
80% task acceptance, pass all applicable post-run checks, and show no dirty-tree
or workspace-boundary violation. Repeat a provider run when results are close to
the threshold or vary materially between runs. Until that evidence exists, do not
start work on:

- ontology or Jev integration;
- complex multi-agent editing or swarms;
- vector retrieval;
- an integration/plugin marketplace;
- remote execution;
- elaborate desktop IDE features.

The evaluation gate requires a meaningful improvement in at least one measured
area: completion, correctness, latency, cost, safety, context efficiency, or
approval requests. Cost is comparable only when explicit rates are supplied;
context token and approval metrics must have the same measurement source. A
change fails the gate if it introduces test/dirty-work regressions, increases a
safety violation, reduces completion by more than one percentage point, reduces
test pass rate by more than one point, or increases comparable latency, cost, or
context use by more than ten percent. `evals/compare.py` implements these
default thresholds; reviewers may tighten them for a high-risk change.

Every report must retain the suite version, commit, lane, provider/model,
per-task results, test results, regressions, turns, tool calls, tokens and their
source, priced cost when configured, wall-clock time, approvals, files read and
modified, context compactions, and repeated failures. Unknown measurements stay
unknown; do not substitute fabricated values. The runner uses automatic approval
in disposable fixture workspaces, so `human_approvals` is zero and approval
requests are reported separately.

Evaluation tasks must be deterministic, license-compatible, small enough to run
locally, and validated independently after the agent exits. Keep provider
credentials out of task fixtures and reports. Live provider runs are opt-in and
may incur cost.
