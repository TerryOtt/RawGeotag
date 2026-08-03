# Updating dependencies

**Cadence: once per photo trip, before you leave.** Not on the road. The whole point
of doing it on a schedule is that a surprise — a crate that changed behavior, a
toolchain that needs reinstalling, a fixture hash that moved — surfaces while you are
at home with the archive, the fixtures and a real network connection, rather than in a
hotel room with a card reader and 2,000 photos waiting to be geotagged.

If a trip is imminent and the update looks at all interesting, **skip it and travel with
the binary you have.** A version that is four point releases behind and verified is
worth more than a current one you have not run against a fixture.

**While you are here, has Lightroom Classic had a major version since last trip?** If
so, run the two checks in [`LIGHTROOM-XMP.md`](LIGHTROOM-XMP.md) — under ten minutes,
and `scripts\lr-xmp-check.ps1` does everything either side of the clicking. Same
reasoning as this file, different trigger: you want to find out that Lightroom moved
while you are at home with the fixtures, not in that hotel room. Dot releases do not
warrant it.

## The short version

```
cargo outdated                   # what is behind, and whether cargo can reach it
cargo update                     # take everything semver-compatible
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
pwsh -NoProfile -File .\scripts\verify-fixtures.ps1   # the one that actually proves anything
git add Cargo.lock && git commit
```

Then read the rest of this file, because two of those steps have a trap in them.

`cargo outdated` is not part of cargo. Install it once with `cargo install
cargo-outdated`.

## Step 1 — see what is behind, and read the columns properly

```
Name                Project  Compat  Latest  Kind    Platform
----                -------  ------  ------  ----    --------
clap                4.6.4    4.6.5   4.6.5   Normal  ---
clap->clap_builder  4.6.2    4.6.5   4.6.5   Normal  ---
gpx->time           0.3.54   0.3.55  0.3.55  Normal  ---
time                0.3.54   0.3.55  0.3.55  Normal  ---
```

| Column | Means |
|---|---|
| `Project` | what your `Cargo.lock` is pinned to now |
| `Compat` | the newest version **`cargo update` can reach** without editing `Cargo.toml` |
| `Latest` | the newest version on crates.io, ignoring your version requirement |

**The one thing to look at is whether `Compat` and `Latest` agree.** Where they do —
as in every row above — the update is free: `cargo update` takes it and the manifest
never changes. Where `Latest` is ahead of `Compat`, cargo *cannot* get you there and
the row needs a hand edit; see the next step.

Rows spelled `a->b` are transitive: `gpx->time` is the `time` that `gpx` pulls in.
Here it is the same crate as our direct `time` and dedupes to one copy, so both rows
close together.

## Step 2 — the `0.x` ceiling, which is the trap

Cargo's rule for pre-1.0 crates is that the **minor** position is where breaking
changes live. So `indicatif = "0.18"` means `>=0.18.0, <0.19.0` and **can never resolve
to 0.19**, no matter how many times you run `cargo update`. Cargo will tell you
"Locking 0 packages to latest compatible versions" — which is true, reassuring, and
entirely consistent with being three minor releases behind.

**A clean `cargo update` is not evidence of being current.** This is not hypothetical:
`indicatif` sat pinned at `"0.17"` here while 0.18 had already shipped six patch
releases, and it stayed that way until a human noticed.

`cargo outdated` is what catches it, because its `Latest` column ignores your
requirement. When a row shows `Latest` ahead of `Compat`:

1. Edit the version string in `Cargo.toml` by hand.
2. Read that crate's changelog — a `0.x` minor bump is a *breaking* release and is
   allowed to have moved anything.
3. Run the full verification below. This is the case where it earns its keep.

**Where the risk actually sits.** The `1.x` deps (`anyhow`, `clap`, `rayon`,
`tempfile`, `walkdir`) are self-correcting — `"1"` keeps picking up 1.x forever. The
exposure is concentrated in the pre-1.0 crates: **`gpx`, `chrono`, `time`, `indicatif`,
`nom-exif`**.

### The workflow pins dependencies too, and `cargo outdated` cannot see them

`.github/workflows/ci.yml` pins its actions by major tag, which never moves on its
own and which nothing in this file's process would otherwise check. GitHub announces
a stale one as a *run annotation* rather than a failure, so it is invisible unless
someone opens a green run and reads it:

```
gh api repos/actions/checkout/releases/latest --jq .tag_name
```

**Ask the API, do not recall the number** — the same rule as the crates above, and it
has already bitten once here in exactly the way that section warns about: a v5 was
recommended from memory on the day v7.0.1 was current. The usual reason a bump is
needed is the runner dropping a Node version, so check what the action runs on
(`using:` in its `action.yml`) rather than assuming the tag is cosmetic.

### The toolchain is not a dependency either

`cargo outdated` sees crates. It does not see rustup, the stable toolchain, or the
MSVC linker every build here goes through, and nothing else in this process would
notice any of them being behind.

rustup answers for both itself and the toolchain in one line:

```
rustup check
```

The MSVC side has no such command, and the obvious substitute is the misleading one.
**`winget upgrade --id Microsoft.VisualStudio.BuildTools` answers from winget's own
source, which lags the Visual Studio release channel** — so "No available upgrade
found" is consistent with being behind, in the same way a clean `cargo update` is. Ask
the channel manifest instead, which is what the VS Installer itself serves updates
from, and compare it against what is installed:

```powershell
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$have = (& $vswhere -products * -format json | ConvertFrom-Json).installationVersion
$want = (Invoke-RestMethod https://aka.ms/vs/18/stable/channel).channelItems |
        Where-Object id -eq 'Microsoft.VisualStudio.Product.BuildTools' |
        Select-Object -ExpandProperty version
"installed $have / channel $want"
```

**Compare those two fields specifically.** The manifest also carries
`info.productDisplayVersion`, which is the same release written the marketing way —
`18.8.2` where `installationVersion` says `18.8.12023.21` — and is the form winget
reports. Comparing one against the other reads as a mismatch on a machine that is
perfectly current. The `18` in the URL is the VS major version, and is the one thing
here that has to be edited by hand rather than asked for.

**On the maintainer's machine a hook runs both of these once per session**, on the
first `cargo build`, and reports which side is behind. It lives in Claude Code's
config rather than in this repo, so it is a convenience on one machine and not part
of the process: the two commands above remain the answer on any other clone.

## Step 3 — verify, and mean it

```
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
pwsh -NoProfile -File .\scripts\verify-fixtures.ps1
```

The release build comes first because `verify-fixtures.ps1` runs
`target\release\rawgeotag.exe` and throws if it is not there.

**If the toolchain or Build Tools moved, `cargo clean` before that build.** Cargo
fingerprints `rustc`, not the linker, so a new MSVC toolset or CRT changes what the
binary is built from while cargo sees nothing to redo. Nothing relinks, and
`verify-fixtures.ps1` — which never builds — then re-checks bytes the *old* linker
produced, and passes. A silent false pass is the one outcome this step exists to
prevent, so spend the half minute rather than work out whether this particular change
needed it.

See [`TESTING.md`](TESTING.md) for what each of those checks is for.

**`cargo test` passing is not enough on a dependency bump.** The unit tests use
synthetic data; the fixtures use real CR3s and NEFs across three timezone cases, and
they compare byte-for-byte output. A dep that changed how a GPX timestamp parses or
how a raw header is read shows up in the aggregate hash and nowhere else, and it costs
under a second — see [`FIXTURES.md`](FIXTURES.md).

### Running `verify-fixtures.ps1`

From the repo root:

```
pwsh -NoProfile -File .\scripts\verify-fixtures.ps1
```

**Use that spelling even though it looks roundabout.** At a cmd prompt the bare
`.\scripts\verify-fixtures.ps1` opens the script in Notepad, prints nothing and returns
`ERRORLEVEL` 0 — a silent skip that reads like a pass; [`FIXTURES.md`](FIXTURES.md) has
the mechanism and the reason `assoc` misleads you about it. Being a `.ps1` it does not
run from a bash prompt either.

No arguments are needed on this machine — the script defaults to the sibling directory
`..\RawGeotag-fixtures` and to this repo's release binary. It writes nothing outside
the fixture tree, touches no photo library, and finishes in about a second.

A healthy run ends with `all fixtures pass` and exit code 0. Each fixture prints what
it is there to exercise, so the output is also the explanation:

```
=== cr3-offset-nonzero ===
    exercises: Streaming read path; EXIF offset +01:00 (real conversion)
    dry run  : wrote nothing  OK
    count    : 2 sidecars  OK
    aggregate: <16 hex digits>  OK
    rehearsal: --dry-run --force left them untouched  OK
```

(`nef-no-offset` prints one more, `gate`, since it is the set whose body records no
EXIF timezone.)

(The real aggregates are in the harness and in [`FIXTURES.md`](FIXTURES.md); they are
deliberately not repeated here, so a legitimate packet change has two places to
update rather than three.)

Any failure prints a red `FAILED:` block listing what went wrong and **exits non-zero**,
so it chains — cmd understands `&&` too:
`cargo test && pwsh -NoProfile -File .\scripts\verify-fixtures.ps1`.

Three things worth knowing before the first run:

- **The raw photos are not in git** — only the script and its per-file
  manifests are. The script expects the tree as a **sibling of the checkout**, so on
  this machine it needs no arguments; from a checkout without one, point at a tree
  that exists:

  ```
  pwsh -NoProfile -File .\scripts\verify-fixtures.ps1 -FixtureRoot <path-to-a-tree>
  ```

  [`FIXTURES.md`](FIXTURES.md) owns the rest: how the tree is laid out, why a second
  checkout needs a second one, and the rebuild recipe if one is ever lost.
- **It deletes `.xmp` files inside the fixture directories**, before and after each run.
  That is required — a leftover sidecar is *skipped* rather than rewritten and would
  silently change the aggregate — and it is confined to the three fixture folders. It is
  also the reason the fixtures live on local `C:\` and not in the archive.
- **`-CheckSources` answers a different question.** It re-hashes every raw against
  the recorded manifests, which distinguishes *the fixture drifted* from *the code
  changed*. Cheap now that the sets are two files each.

Execution policy is `RemoteSigned` here, which runs local scripts without complaint. If
you ever get the fixtures from a downloaded zip rather than a copy, Windows marks the
files as remote and you would need
`pwsh -NoProfile -Command "Unblock-File .\scripts\verify-fixtures.ps1"` first.

### If a fixture hash moves after an update, that is a regression

This is the one place where the usual advice inverts. `FIXTURES.md` says a changed
aggregate can be legitimate — a deliberate edit to the XMP packet, or a crate version
bump landing in `x:xmptk`, moves all three hashes and the right response is to
re-derive them.

**Neither of those applies here.** A dependency update changes no packet content and no
crate version, so the output has no business changing. If a hash moves, **something in
a dependency altered the coordinates or the document, and you want to know which**.
Bisect the update — revert `Cargo.lock`, then re-apply one crate at a time with
`cargo update -p <crate>` — rather than accepting the new number.

**A toolchain or Build Tools upgrade is the same case**, by the same argument: it moves
neither the packet nor a crate version, so all three aggregates must come back
unchanged. There is no lockfile to bisect, so the question to answer instead is whether
the previous binary was built by the previous linker — which is only a meaningful
question if you ran `cargo clean`, and is the second reason Step 3 insists on it.

## Step 4 — commit the lockfile

`Cargo.lock` is committed in this repo, which is what makes an update a thing that
happened on a date rather than a thing that drifts. Commit it on its own, with the
before-and-after versions in the message, so a later bisect has something to aim at.

## One checkout on this machine

`C:\Travel\RawGeotag` is the single source of truth. There used to be a second clone
under `C:\Users\...\Claude\`; it was removed on 2026-08-02 along with its own fixture
tree, because keeping two in step was pure overhead for one maintainer.

**If a second one is ever created, updating one leaves the other stale** — and the
stale one reports the same rows from `cargo outdated` as though nothing had been done,
which is what makes it worth writing down. Update either, push, then `git pull` in the
other and rebuild there too: the release binary is per-checkout, as is the fixture
tree beside it.
