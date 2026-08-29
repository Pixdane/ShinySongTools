# Bootstrap timing probe

Status: one exact-SHA live run completed; the initialization-race hypothesis
is supported. The fixed delay is not accepted as a production readiness gate.

## Question

The first production bundle reached `scsp-bootstrap` and then aborted while
the worker called `il2cpp_domain_get` concurrently with the main thread's
`il2cpp_init`. Does moving that same single call five seconds later avoid the
initialization-order crash?

## Scope

This probe changes one variable: after the exact UnityFramework image and
export table have been accepted, the worker waits five seconds before the
single `il2cpp_domain_get` call.

It then terminates. It does not:

- attach the worker to IL2CPP;
- initialize the metadata cache;
- resolve the LateUpdate target;
- read or write a MethodPointer;
- construct or publish the App;
- install any hook;
- start the Debug socket.

The fixed delay is diagnostic only. A successful run would support the timing
race diagnosis, but it would not establish a production readiness gate.

## Build

```sh
bb bootstrap-probe build
```

The Babashka task delegates the non-Rust artifact graph to
`zig build bootstrap-timing-probe`.

The signed candidate and sidecar are published under:

```text
build/experiments/bootstrap-timing-probe/
├── AKInterface.bundle/
└── AKInterface.bundle.manifest.json
```

No build or fixture command reads, modifies, starts, or attaches to the game.

`bb bootstrap-probe status` is read-only and uses the experiment candidate for
the candidate comparison. `bb bootstrap-probe patch` reuses the normal
transactional installer but remains approval-gated exactly like
`bb bundle patch`.

## Live safety boundary

A live run requires a separate approval bound to the built executable SHA-256.
The proposed batch is one stage and one launch, at most 30 seconds, with no
attach/sample/vmmap, no metadata/cache work, and no MethodPointer access. A new
DiagnosticReport, abnormal exit, identity/preflight drift, or the time limit is
an immediate stop condition. Restore and residue audit are mandatory.

## Live result — 2026-08-29

The approved batch was bound to source commit
`5bfb083de1d638c0ee65ac05c09dd109e67bc370` and candidate executable SHA-256
`8b53d7301286d109019a0d728ab9aef0136a2cef5485fc7fd8dcc0809ba5d542`.
Preflight was stopped/clean with a valid candidate signature and no residue.

The candidate was staged once and launched once as PID `14768`. Unified log
evidence recorded image identity at `19:38:51.616`, armed the 5000 ms delay at
`19:38:51.649`, and recorded `domain_get returned non-null` followed by probe
completion at `19:38:56.650`. The process survived the probe. No attach,
sample, vmmap, metadata/cache work, MethodPointer access, second launch, or new
DiagnosticReport occurred.

The game was stopped immediately after the result. Mandatory restore returned
the installed executable to baseline SHA-256
`9a2327e533deb0a8b7643ba2c1c0e28c945c19829c302c48609531d8b3c57f18`;
the final audit was stopped/clean, signature valid, and residue none. The local
machine-readable record is kept under the gitignored
`artifacts/bootstrap-timing-probe/live-20260829-01/` directory.

This result supports the diagnosis that the first production attempt called
`il2cpp_domain_get` during `il2cpp_init`. It does not make a fixed sleep a
production solution: the production bootstrap still needs a real readiness
contract before any IL2CPP API call.
