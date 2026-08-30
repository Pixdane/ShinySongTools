# translation_dump

Development-only text collection for the future translation plugin. Live
validated on game 2.17.0 (see `experiments/translation-dump-live-validation.md`).

The plugin hooks two `LocalizationManager` entry points with the
entry-patch mechanism (the MethodPointer slot swap cannot intercept
AOT-compiled direct calls):

- `GetTextOrNull(string, int)` — upstream scsp-localify's replacement point.
- `GetText(string, int)` — the live UI text path on the iOS build (896 direct
  call sites versus 10 at recon time).

Each replacement calls the original exactly once, returns the original
`System.String` unchanged, and copies `(category, id, text)` into a bounded
fixed-size UTF-16 record; it never allocates on the hot path, locks, or
performs file I/O. The main-thread Update system merges records and atomically
replaces:

```text
<DataRoot>/shiny-song-tools/dumps/localify.json
```

The JSON shape is compatible with scsp-localify's localization dictionary:
`category -> id -> original text`. Existing data is loaded and merged at
startup, so the table accumulates across sessions. Records larger than the
documented bounds are skipped whole and reported through diagnostics rather
than truncated.

Enable it with:

```toml
[translation]
dump = true
```

## Class-2 / class-3 capture (lyrics, scenario, data files)

Three more entry-patch hooks extend collection beyond the Localify table:

- `PRISM.DataFile.GetBytes(string)` — passive: every `.json` data file
  flowing through the game's data store is written to `dumps/json/`
  (bounded to 1 MiB per file, deduplicated by file existence). Captures
  scenario text, master data, camera/motion data — the whole class-3 layer.
- `LiveMVOverlayView.UpdateLyrics(string)` and
  `TimelineController.SetLyric(string)` — lyric line capture into
  `dumps/lyrics_dump.json` (original text keys). These fire on the DMM
  build; on the iOS build the lyric display path differs and capture is
  currently zero (under investigation).

A relocation-capable trampoline (cbz/cbnz/b.cond/tbz/tbnz/b/bl re-encoded
with variable-length sequences; adr/adrp/literal loads still fail closed)
lets these hooks install on prologues the plain copy would reject.

`translation_dump.scenariodump {"limit": 25}` proactively bulk-dumps
scenario text: it derives scenario ids from the dic's `mlADVInfo_Title_*`
keys, probes `{sid}_{NN}.json` via the game's own `DataFile` API and writes
each file. 1,533 sids / 3,461 files covered on game 2.17.0. Each call runs
synchronously on the main thread — the game freezes briefly; keep batches
small and repeat until `done`.

## Whole-table dump

The per-call hooks only see text the game actually looked up. The complete
loaded table lives in the manager's `dic` field
(`Dictionary<string, Dictionary<int, string>>`). Once any dump hook has fired
(the manager instance is captured from the hook's `this`), the debug topic

```sh
bb debug translation_dump.dicdump '{}'
```

walks that dictionary manually (Newtonsoft's serializer is stripped from the
iOS build) and merges the whole table into `localify.json` — 5,613 categories
/ 137,124 entries in one shot on game 2.17.0, versus 44,860 entries in the
upstream community dump.

When Debug is enabled, `translation_dump.status` exposes per-entry-point hit
counts plus capture/drop/flush counters, and `translation_dump.flush` requests
an immediate main-thread flush.
