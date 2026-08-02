# Code quality: no broken windows on main

## The standing order

> **A branch can be as ugly as it needs to be. `main` has no broken windows.**

The asymmetry is the whole policy, and both halves are load-bearing.

**On a branch, be as messy as the problem requires.** Spike it, hard-code it, copy
and paste it, leave the ugly `if` chain, skip the tests while you are still finding
out what the thing even is. Exploration that has to look presentable while it is
happening is exploration that gets abandoned early. Nobody is reviewing your branch.

**Then clean it before it goes near `main`.** Not "mostly clean", not "clean apart
from that one bit". The bar is that a reasonable reviewer reading the diff cold finds
nothing to wince at — no cringe, no debt, no "I'll fix that later", no commented-out
experiment, no leftover `dbg!`. If you would apologise for a line in the PR
description, that line is not ready.

## Why "broken windows"

From *The Pragmatic Programmer* (Hunt & Thomas), borrowing the criminology
observation: a building with one unrepaired broken window is soon a building with
none intact. One is a signal that nobody cares, and once that signal is up, the
decline is fast and nobody feels responsible for it.

Code does the same. One tolerated shortcut on `main` is a licence for the next one,
and the third arrives without anyone deciding. **The cost is not the shortcut; it is
the precedent.** Repairing a window is cheap. Re-establishing that windows get
repaired, after a year of not, is not.

The book's other half is worth keeping too: **if you genuinely cannot fix it now,
board it up.** Damage that is visibly contained does not send the signal. A named
`TODO` with a reason and a bound is a boarded window; the same code with no comment
is a broken one.

## The gate, honestly

There is one maintainer, and the workflow is commit straight to `main` — no branch,
no PR. **So the gate is self-review at commit time, and it is the same bar.** "Would
this survive a reviewer" is not softened by there being no reviewer; it is the only
thing standing in for one.

### What GitHub enforces

Two rulesets on `main`, added 2026-08-02. They are deliberately separate, because
bypass is per *ruleset*, not per rule — one combined ruleset with the maintainer on
its bypass list would have exempted him from the force-push block too.

| Ruleset | Rules | Bypass |
|---|---|---|
| `main: require pull request` | `pull_request` — 1 approval, **code-owner review required**, **squash the only permitted merge**, stale reviews dismissed on push, last push must be approved | repository admin, always |
| `main: no force-push or deletion` | `non_fast_forward`, `deletion` | **none — binds the admin as well** |

**"One approval" is not the same as "the maintainer's approval"**, and the difference
only shows up once there are two collaborators who could rubber-stamp each other. So
`.github/CODEOWNERS` assigns every path to the maintainer and the ruleset requires a
code-owner review, which makes his approval specifically mandatory. Two related
switches are on for the same reason: `dismiss_stale_reviews_on_push` drops approvals
when new commits land, and `require_last_push_approval` stops someone approving a
branch and then pushing to it.

A code owner cannot approve their own pull request — which never bites here, because
the maintainer bypasses this ruleset and commits straight to `main`. A code owner also
needs write access or the rule silently matches nobody; check that before adding
anyone to `CODEOWNERS`.

### Every merge is a squash

**One pull request becomes exactly one commit on `main`.** Set in two places on
purpose, because they fail differently: the ruleset restricts `allowed_merge_methods`
to `squash` for `main` specifically, and the repository settings switch off merge
commits and rebase merges outright so the other buttons are not even offered. The
ruleset is the enforcement; the repo setting is what stops someone reaching for a
button that would then be refused.

The squash commit is configured to take its **title from the pull request title** and
its **body from the pull request body**, rather than concatenating every "wip" and
"fix typo" message from the branch. That is deliberate given how much of this
project's reasoning lives in commit messages — a squashed history is only an
improvement if the surviving message is the considered one.

None of this touches the maintainer's workflow, which bypasses pull requests
entirely. It governs what arrives from anyone else.

### A merged branch is deleted immediately

**Standing order: the moment a branch is merged, it is gone.** A merged branch that
lingers is a broken window of the housekeeping kind — after a few of them nobody can
tell at a glance which branches are live, and the signal that goes up is that nothing
here is tended.

`delete_branch_on_merge` is **on**, so GitHub deletes the head branch automatically
the instant a pull request merges. That is the forced part, and it needs no
discipline from anyone.

**Two cases it cannot reach, which are therefore standing orders rather than rules:**

- **Branches on a fork.** GitHub deletes branches in *this* repository; a contributor
  working from their own fork owns that branch and only they can remove it. Delete it
  after your PR merges.
- **Branches that are never merged.** An abandoned spike, or a PR closed without
  merging, leaves the branch behind and no setting fires. Delete it when you abandon
  it — the point at which you know it is dead is the point at which you are the only
  person who knows.

Locally, `git branch -d <name>` refuses anything not merged, which is the safe form;
`git fetch --prune` clears remote-tracking refs for branches GitHub has already
removed.

**Deliberately not automated:** a scheduled job that reaps stale branches. It is
standing infrastructure with a permanent carrying cost, aimed at a repository that
has had exactly one branch its whole life. That is the same trade declined for the
Lightroom plugin and for hosting the fixtures — if branch clutter ever becomes real,
revisit it then.

**The first is a no-op today and that is the point.** The repository is public with
one collaborator, so a non-collaborator already cannot push at all — GitHub refuses
it and their only route is fork-and-PR. The ruleset exists so that stays true the
moment anyone is granted write access, rather than depending on nobody having been.

**The second is the one with teeth**, and it protects against the only account that
can actually damage `main`: the maintainer's. Rewriting or deleting published history
is now refused by the server rather than by remembering not to — which is the same
preference for making a mistake impossible that runs through the rest of this project.

Note that "Claude" is not a separate actor to allow-list. Commits are authored and
pushed as the maintainer, with Claude recorded in a `Co-Authored-By` trailer, so
GitHub sees one identity and any rule permitting the human permits the assistant.

## What counts as a broken window here

Derived from a real review pass over this codebase, not from a style guide. Every
row below is something that was actually found and fixed on 2026-08-02, which is why
these and not the usual generic advice:

| Shape | The instance |
|---|---|
| Reimplementing what a dependency already gives you | a hand-rolled scratch-directory type, duplicated in two modules, while `tempfile` was already a dependency **and cited in `Cargo.toml` for exactly those properties** |
| A function long enough to hide its own control flow | `run()` at 159 lines with a dozen mutable accumulators, one of them incremented in two loops 55 lines apart |
| Two types modelling the same thing differently | `Extraction` repeated `path` in every variant; its neighbour `Written` did not |
| Passing an owned value by reference, then cloning out of it | a `PathBuf` cloned per photo because the function took `&Photo` |
| Rebuilding a constant inside the loop | `GapLimits` reconstructed per photo, because `&Args` was threaded in instead of resolved settings |
| `pub` that buys nothing | a type nothing outside the module could construct or receive |
| The same normalisation written twice, with a loose primitive | `trim_start_matches('.')` where `strip_prefix('.')` was meant, in two modules |
| A data table whose shape the code does not honour | `extensions()` returns a slice; the directory walk matched only the string the user typed |
| A module reaching up into the binary root | `track.rs` calling `crate::format_utc` |
| A runtime assertion where the type system would do | `unreachable!` guarding a state that consuming the value made unrepresentable |
| Dead conditions | `clamp(-1.0, 1.0)` on a value that cannot go negative |
| Over-permissive parsing | an offset parser that accepted `+0:0:00` and `+::0700` |
| Comments describing constraints that do not exist | a note about borrow ordering on a type that is `Copy` |

**The recurring test, if you want one line:** would an experienced reviewer, reading
this cold, have to ask why? If yes, either change the code or write down the answer.

## What is *not* a broken window

The policy is not licence to relitigate. Specifically:

- **A settled decision you would have made differently.** `PLAN.md` and `CLAUDE.md`
  record several with their reasoning, some marked *do not re-propose*. Disagreeing
  is fine; say so explicitly rather than quietly diverging. Reopening one needs new
  evidence, not fresh taste.
- **Deliberate simplicity.** A flat `Vec` and a linear scan are not debt when the
  input is bounded by what a human types on a command line. Speculative generality
  is the defect, not its absence.
- **A recorded gap.** `TESTING.md` lists branches left uncovered with the reason for
  each. A known gap with a written reason is a boarded window and stays boarded until
  the reason stops holding.
- **Verbosity that buys clarity.** This project takes the obvious mechanism over the
  clever one on purpose. Longer and duller is not a window.

## A review is always all four

**"Do a deep dive review" means all four of these, every time, without being asked
for them separately:**

1. **Code**
2. **Unit tests** — held to [`TESTING.md`](TESTING.md), not merely "they pass"
3. **Code comments** — in the code *and* in the tests
4. **Docs** — `docs/` *and* `CLAUDE.md`

They are one request, not four, because they fail together. Removing the extension
argument left stale invocations in the README, a comment naming a function that no
longer existed, and a sample output that no longer matched. Reviewing one dimension
and not the others just moves the broken window somewhere less visible.

The same applies in the other direction. **Changing any one of the four is reason to
look at the other three**, because a change rarely stays in its lane:

| A change to… | …routinely stales |
|---|---|
| code | tests pinning the old shape; comments naming what moved; every doc showing a command or a sample output |
| unit tests | the comments inside them, which are what explain why a case exists at all; and `TESTING.md`, if a gap opened or closed |
| comments | nothing else, but they drift from the code faster than anything else here |
| docs | little, though a fact corrected in one usually belongs in a code comment too |

**The test dimension is not "did they pass".** A single day produced a test that
passed its own mutation check and therefore guarded nothing, a test wholly subsumed
by its neighbour, and a bulk edit that silently deleted ten of them — none of which a
green suite would have mentioned. Every one of those was found by reading the tests
as an artifact in their own right.

## Before you push to main

Code is one of three standards and the other two have their own files. All three
apply to the same diff:

- **this file** — the code itself
- [`TESTING.md`](TESTING.md) — reach for every branch, and prove every test can fail
- [`WRITING.md`](WRITING.md) — every document leads with what its reader came for,
  and comments explain *why*, not *what*

Mechanically, that is `cargo clippy --all-targets -- -D warnings`, `cargo test`, and
`.\scripts\verify-fixtures.ps1` — plus the determinism re-run if the change touched
the phase structure, the outcome enums or the reporting order. A green suite is the
floor, not the bar: **clippy has no opinion about any row in the table above.**

### What runs automatically

Two layers, and the second is not redundant with the first:

| | Where | Runs | Skippable |
|---|---|---|---|
| `.githooks/pre-commit` | your machine, before the commit exists | fmt, clippy, test | `--no-verify`, and only present if the clone was wired up |
| `.github/workflows/ci.yml` | GitHub, on push to `main` and on every PR | the same three | no |

The hook is the layer that saves you time, because it catches a problem before it is
in the history. CI is the layer that cannot be talked out of it, and the only one
that sees a pull request from a fork.

**Wire the hook up once per clone** — git does not track `.git/hooks`, so a hook
living only there protects nothing on a fresh checkout:

```
git config core.hooksPath .githooks
```

It skips the Rust checks entirely on a docs-only commit, which is most of them here.

**Neither layer runs `verify-fixtures.ps1`**, because it needs 222 MB of raw
photographs that are not in version control — it would fail on any clean checkout,
and a check that fails for the wrong reason is how people learn to type
`--no-verify`. Fixtures stay a manual, local step, and they are the ones that prove
output has not moved. Run them before anything that touches what gets written.
