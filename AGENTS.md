# AGENTS.md

Guidance for AI coding agents working in this repository. Read this before making changes.

## Project Overview

Shiny Song Tools ports the functionality of [chinosk6/scsp-localify](https://github.com/chinosk6/scsp-localify) to the iOS version of 偶像大师 闪耀色彩 running on macOS via [PlayCover](https://playcover.io). The hook/injection approach involves Frida (`frida-tools`) and `insert-dylib`.

## Environment Rules (strict)

- **Prefer tools provided by the Nix devshell.** Tools declared in `flake.nix` (e.g. `bb`, `zig`, `frida`, `insert-dylib`) take priority over anything else when both exist.
- **Never install new tools.** Forbidden: `brew install`, `pip install`, `cargo install`, `npm install -g`, downloading binaries into the repo, etc. Compilers and toolchains that already exist on the host machine may be used; only *installing* new ones is off-limits.
- If a tool is missing, the only correct action is: add it to `flake.nix` (`devShells.default.packages`) and let the user rebuild the shell. Ask the user before mutating `flake.nix`.
- **Nix commands that would fetch packages belong to the user.** Nix operations that need no new downloads (environment unchanged, everything already in the store) are safe to run. But after a `flake.nix` package change, nix must fetch new packages: that is slow and network-dependent, and agent-triggered fetches can hang or leave background nix processes holding locks. After modifying packages, hand the nix command to the user instead of running it yourself.
- **Clean up stray nix processes.** If agent-triggered nix processes are left running, kill them (`ps aux | grep nix`). Never kill the user's own interactive devshell: user shells are attached to a TTY, while stray agent processes show `??` as TTY.
- Exception: the Rust toolchain itself is managed via `rustup` (provided by the devshell) pinned by `rust-toolchain.toml`. Do not run `rustup` commands that change or add toolchains; just use the pinned one.

## Languages & Build Systems

| Purpose | Tool | Rules |
|---|---|---|
| Primary development language | **Rust** | New code defaults to Rust. `cargo fmt` must pass; `cargo clippy` and `cargo test` must pass before finishing. |
| Scripting / automation | **Babashka (Clojure)** | All scripts are Clojure run by `bb`. Task entry points are declared in `bb.edn` - prefer adding a task there over ad-hoc script invocations. No Bash/Python/Node for new scripts. |
| Building non-Rust artifacts (C/ObjC/etc.) | **Zig** (`build.zig`) | Anything that is not built by cargo or run by bb goes through a `build.zig` target. Do not introduce make, cmake, autotools, or xcodegen for these. |

**Quality gates apply to production code only** (crate root / `src/` / `tools/`). Code in `experiments/` is throwaway validation and does not need to pass fmt/clippy/test.

## Experiments (`experiments/`)

Experiments follow **all rules above** (environment, language, safety). Only two things are relaxed for them:

- **Quality gates are relaxed:** no `cargo fmt` / `clippy` / `test` requirements; throwaway code is fine.
- **Build wiring is relaxed:** experiment code does not need to be wired into `cargo` / `bb.edn` / `build.zig`; compile/run it ad hoc, with outputs still going to `build/experiments/<experiment-name>/`.

Lifecycle rules that always apply:

- **Close out every experiment.** When an experiment finishes, update and tidy its files and documentation according to the outcome: if it produced meaningful results, persist the important outputs (findings into docs, reusable code into the proper production location); if it failed, discard the related files.
- **One experiment = one unit of organization.** An experiment with documentation only is a single markdown file (`experiments/<name>.md`). An experiment with supporting files gets its own folder (`experiments/<name>/`) with the doc inside.
- **Decide git tracking deliberately.** Check whether each experiment's files belong in git. Files that must not be committed (large binaries, game-derived data, local-only scratch) go into `.gitignore` - add the ignore rule in the same change, not later.
- **Keep experiment files tidy.** Build outputs of experiments go into `build/experiments/<experiment-name>/` (the repo-root `build/` directory, gitignored), not scattered next to sources. Even though such files are not committed, keep them organized and clean up stale outputs regularly.

## Common Commands

```sh
nix develop            # enter the devshell (or rely on direnv)
bb tasks               # list available Babashka tasks
bb <task>              # run a task declared in bb.edn
zig build              # build non-Rust artifacts
cargo fmt && cargo clippy && cargo test   # Rust quality gates
```

## Game & Injection Safety

- **Attaching to or injecting into the running game requires explicit user approval, every time.** This includes `frida` attach/spawn, `insert-dylib` on the game binary, and launching the game for hook testing. The game embeds nProtect AppGuard; detection can lead to irreversible account bans. Never run these on your own initiative.
- **One explicit user approval MAY authorize a predeclared, bounded batch of operations/attempts**, including multiple launches and, only if explicitly enumerated, specific modification/staging or live-task sample operations. Do not prompt again for each operation inside that approved batch while execution remains within the declared variants, attempt caps, operation types, stop conditions, and unchanged safety assumptions. Pause and obtain a new approval if scope expands, a new dangerous operation type was not included, patch/preflight drift or failure occurs, an unexpected phenotype requires an unapproved attach/sample, or the batch is exhausted. This never permits unbounded blanket approval.
- **Consecutive runtime attempts of the same variant may retain the existing patched/staged state when patch identity is unchanged.** Stop the game process after each attempt, record evidence, and run full preflight before every next launch. Do NOT restore and re-patch between same-variant attempts.
- **Restore is mandatory when switching variants, on patch/preflight drift or failure, when the user stops the experiment, or at final closeout/residue audit.**
- **A game modification/staging approval is needed only when external patch/staging state is actually changed.** It is not re-requested merely to reuse an unchanged valid patch. Within an approved batch, modification/staging and live-task sample/attach operations are covered only when explicitly enumerated in that approval; otherwise they each require their own approval.
- **Game-derived files live only in `artifacts/` (gitignored).** IL2CPP dumps, decrypted `global-metadata.dat`, extracted bundles/assets, copies of the game binary: all go there, never elsewhere in the repo, never committed.

## Conventions

- **All build outputs go to the repo-root `build/` directory** (gitignored), organized per component inside: `build/target/` for cargo, `build/zig/` for zig build artifacts, `build/experiments/<experiment-name>/` for experiment outputs. Cargo is redirected there via `.cargo/config.toml`; zig targets must set their install prefix there. Never scatter build artifacts next to sources.
- Keep the repo self-contained: anything a fresh `nix develop` + `bb` + `cargo` + `zig build` cannot reproduce should not exist. Game-derived files under `artifacts/` are not shipped with the repo, but the *process* that produces them must be reproducible: the tools/scripts/programs that generate any artifact there are persisted in the repo, so the artifact can always be regenerated from the game files.
- Platform targets are macOS only (aarch64-darwin, x86_64-darwin).
- When a change affects build or dev environment, update `flake.nix` / `bb.edn` / `build.zig` in the same change, not as a follow-up.
- Never hardcode personal paths or account-identifying values in code or scripts: game container paths (`~/Library/Containers/...`), bundle IDs, user names, account identifiers. Read them from a config file or environment variable instead, and use placeholders in examples and tests.
