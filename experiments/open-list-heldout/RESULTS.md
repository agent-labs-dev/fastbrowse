# Open list held-out A/B

Does the open list text in Jev's operation and target questions (90980a47) help on lists it was not developed on?

**Verdict: helps, on a narrow base.** It is a general change, not eval tuning. Correct answers were 13/18 with it and
11/18 without it, at $1.38 against $2.08. The whole gain comes from one task, with three runs per task.

The fact-only text (`fact-only.patch`) does not perform the same. It also scored 13/18, but it lost the gain on the
task the text changed. Its extra correct runs came from a task where the text cannot explain them.

## Method

- Six tasks on sites in no suite, frozen in commit 6250f8b9 before any run. Answers came from `ground_truth.py`.
- `without` is 003b953b, `with` is 90980a47, `short` is 90980a47 with `fact-only.patch`. Three runs per task and build,
  Browser Use Cloud browser, default settings. `short` ran after the other two.
- Tasks and answers were not edited. The grader was: `grade.py` passed any answer holding the expected number, and
  citation markers such as `[5]` made a stated 6 pass. `regrade.py` reads the count the answer states, and every table
  here uses it. Unchanged, `grade.py` gives 15/18 for both builds.
- Dollars are the runs' own cost lines, browser and proxy included. Total $5.62: $3.46 for the A/B, $2.16 for `short`.

## Results

| build | correct | status complete | median steps | median seconds | dollars |
|---|---|---|---|---|---|
| without | 11/18 | 11/18 | 11 | 47 | 2.075 |
| with | 13/18 | 12/18 | 10 | 42 | 1.381 |
| short | 13/18 | 11/18 | 10.5 | 42 | 2.159 |

| task | build | correct | stated | status | steps | seconds | dollars |
|---|---|---|---|---|---|---|---|
| datatable-client-pages | without | 3/3 | [[12], [12], [12]] | complete 3 | [9, 8, 7] | [51, 39, 36] | 0.186 |
| datatable-client-pages | with | 3/3 | [[12], [12], [12]] | complete 3 | [8, 19, 10] | [39, 86, 44] | 0.285 |
| datatable-client-pages | short | 3/3 | [[12], [12], [12]] | complete 3 | [7, 4, 7] | [40, 31, 35] | 0.153 |
| django-next | without | 3/3 | [[7, 2023], [7, 2023], [7, 2023]] | complete 3 | [13, 13, 13] | [50, 49, 48] | 0.310 |
| django-next | with | 3/3 | [[7, 2023], [7, 2023], [7, 2023]] | complete 3 | [9, 9, 17] | [35, 34, 76] | 0.324 |
| django-next | short | 3/3 | [[7, 2023], [7, 2023], [7, 2023]] | complete 3 | [13, 9, 13] | [45, 34, 66] | 0.265 |
| gutenberg-next | without | 1/3 | [[5], [6], [6]] | complete 3 | [8, 3, 3] | [43, 14, 18] | 0.076 |
| gutenberg-next | with | 1/3 | [[6], [5], [6]] | complete 3 | [3, 10, 3] | [14, 37, 16] | 0.063 |
| gutenberg-next | short | 3/3 | [[5], [5], [5]] | stuck 1, complete 2 | [16, 6, 6] | [44, 34, 37] | 0.124 |
| products-numbered | without | 1/3 | [[7], [10], [6]] | stuck 3 | [86, 125, 36] | [378, 473, 129] | 1.170 |
| products-numbered | with | 3/3 | [[6], [6], [6]] | stuck 3 | [33, 33, 30] | [171, 121, 108] | 0.412 |
| products-numbered | short | 1/3 | [[7], [6], [10]] | stuck 3 | [53, 36, 161] | [194, 136, 559] | 1.158 |
| shop-numbered | without | 3/3 | [[10], [10], [10]] | complete 2, stuck 1 | [25, 15, 16] | [114, 72, 46] | 0.300 |
| shop-numbered | with | 3/3 | [[10], [10], [10]] | complete 3 | [23, 23, 23] | [91, 78, 81] | 0.257 |
| shop-numbered | short | 3/3 | [[10], [10], [10]] | complete 3 | [35, 16, 12] | [136, 84, 66] | 0.420 |
| tablets-load-more | without | 0/3 | [[], [], []] | stuck 3 | [9, 9, 9] | [18, 18, 18] | 0.033 |
| tablets-load-more | with | 0/3 | [[], [], []] | stuck 3 | [9, 9, 9] | [20, 20, 22] | 0.041 |
| tablets-load-more | short | 0/3 | [[], [], []] | stuck 3 | [9, 9, 9] | [20, 18, 21] | 0.040 |

Truth: datatable 12, django 7, gutenberg 5, products 6, shop 10, tablets 8.

## Reading the results

- **products-numbered** is where the text acted. Every run on every build ended `stuck`, including the ones with the
  right count. With the text, runs took 30 to 33 steps and all stated 6. Without it they took 36 to 125 steps and
  stated 7, 10 and 6. Fact-only matched `without` (36 to 161 steps, 7, 6 and 10, $1.16). The site reports 28 results
  but its pages end at 25, which is the likely reason no run closed the list. That cause is unverified.
- **shop-numbered**: all correct. Rejected DONEs were 0 with the text and 16 without it, and `without` ended `stuck`
  once. Steps were 23 in each `with` run against 15 to 25 without it.
- **datatable-client-pages** and **django-next**: no difference in correctness. `with` ran 19 steps once on datatable.
- **gutenberg-next**: not attributable to the text. `with` never sent it here because no list opened, and its runs
  match `without` at 1/3. The wrong runs went to the site search, which counts a book outside the shelf. In `short` a
  list opened in all three runs and all were right, but a list opens before the text can act, so this looks like run
  to run variation. It shows how far three runs can swing.
- **tablets-load-more**: 0/9 on all builds. The button is an `<a>` with no `href`, read as a paragraph and never
  offered as a control, so no question text can reach it. The text was sent and Jev scrolled until `no_progress`.

Load more is untested as a result: no build ever had the control to pick. This is the one pagination kind the
experiment could not measure.
