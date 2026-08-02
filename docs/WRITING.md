# Documentation standard

## The standing order

> **Every document leads with what its reader came for. Everything else comes after.**

For the README that reader is the 98% case — someone deciding whether this tool is
for them and then trying to run it. They get *what it does*, then *how to run it*,
and nothing stands between those two. Philosophy, compatibility notes, benchmarks and
verification evidence all matter, and all of them come later.

**The rule generalises past the README, but the reader does not.** Nobody reads
`UPDATING.md` casually. Applying "write for the 98% user" to a maintainer document
would be as wrong as burying the README's first command — the point is not that every
document is for beginners, it is that every document opens with the thing *its own*
reader arrived wanting.

So the first question for any document is **who opens this, and what were they after?**

| Document | Its reader | What they came for |
|---|---|---|
| `README.md` | someone evaluating or running the tool | what it does, then a command that works |
| `PLAN.md` | someone changing the design | the settled design |
| `TESTING.md` | someone adding, changing or removing a test | the bar a test has to clear |
| `FIXTURES.md` | someone running or rebuilding the fixtures | the command, then the rationale |
| `UPDATING.md` | someone about to bump a dependency | whether to update at all, then how |
| `LIGHTROOM-XMP.md` | someone re-running the comparison after a Lightroom upgrade | the procedure |
| `CLAUDE.md` | Claude, at the start of every session | the binding constraints |
| this file | someone writing or reviewing a document | the standing order |

Get that wrong and the document reads as though it were written for its author. Both
corrections made on 2026-08-02 were exactly that mistake: the README opened with 122
lines of reasoning before a runnable command, and `PLAN.md` opened with two sections
marked **COMPLETE** before any design.

## Rules

1. **Lead with the reader's goal.** Rationale, history and evidence go after it — or
   into an appendix if they are finished business.
2. **One canonical place per fact.** Where a summary must repeat something, it names
   its source and the two are corrected together. Carry the *caveat* across, not just
   the number; a stale caveat is what has actually drifted here, twice.
3. **No hand-maintained counts.** Test totals, sidecar tallies, "done three times so
   far" — all of them go stale, and the test count did it three separate times.
   Name the command that answers the question instead.
4. **Record decisions and their reasons, not restatements.** "Prints the summary"
   above `print_summary` is noise. Why the column is seven characters wide is not.
5. **Correct by appending, never by rewriting.** A note saying what was previously
   claimed and why it was wrong is worth more than a clean-looking record — the
   reader learns the shape of the mistake, which is what stops it recurring.
6. **Numbers ≥ 1,000 carry thousands separators**, in prose as well as in program
   output. Exceptions: Rust literals, text quoted verbatim from another tool so it
   stays greppable, and years, model numbers, offsets and coordinates.

## Comments are documentation too

Same rules, one addition: a comment earns its place by explaining something the code
cannot. Names and signatures already say *what*; comments are for *why this and not
the obvious alternative*, and for the trap that is invisible at the call site.

The bar that has worked: **would a reader otherwise repeat this mistake?** A bare
`paths.sort_unstable()` needs a comment because deleting it breaks nothing that fails
loudly. `fn print_summary` does not need one.

## Signals you have buried the lead

Cheap to check, and each one has actually happened here:

- The first runnable command is below the fold.
- The reader must scroll past a section marked *COMPLETE* or *retained as a record*.
- Two consecutive paragraphs make the same move — a scope caveat stated twice, a
  rule restated three times for emphasis.
- A "that last one" or "the table above" that no longer points where it did, because
  something was appended between them.
- A section's heading covers only its first third.
- A file is loaded into every session and nobody can say what it would cost to lose
  any given paragraph.

## When a document outgrows its home

`TESTING.md` and this file both began inside `PLAN.md`. The signal was the same in
both cases: a section had become a fifth of a document whose subject it was not, and
it was still growing.

**Move it, do not copy it.** Leave a short pointer where it was, so the one-canonical-
place rule survives the split.
