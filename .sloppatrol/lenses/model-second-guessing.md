# model-second-guessing

## Defect

Hidden logic that overrides or filters what a model said, or decides in its place, by a rule the model is
never told: a keyword or regex list that rejects claims or answers, a confidence bar that silently turns a
model's choice into a different action, a heuristic that rewrites the model's output, a site- or
task-shaped special case, a post-filter dropping options before the model sees them, a fallback that
replaces a model answer with a guess. The model cannot learn the rule from its prompt, so it keeps
producing what the rule rejects.

## Do not flag

- Safety and authorization invariants: irreversible-action authorization, secret origin scoping,
  citation checks that a quote really appears on the page.
- Schema validation of a reply against the schema the model was sent.
- A threshold the prompt or question states to the model, or one applied to a documented probability
  (Jev's calibrated choice confidence) with the reason recorded next to it.

## Judgment

Does the model get told this rule, and would it behave differently if it were? Would the run do better
handing the model the information and letting it decide?

## Output

On the rule. `what` names the model output overridden and the rule; `why` names the failure it can cause
and whether the prompt states the rule.
