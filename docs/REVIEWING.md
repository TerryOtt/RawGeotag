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

If contributors ever arrive this becomes an actual PR gate and nothing else changes.

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
