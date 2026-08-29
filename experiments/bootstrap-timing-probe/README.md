# Bootstrap timing probe

Status: prepared for no-game validation; no live run has been approved or
performed.

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
