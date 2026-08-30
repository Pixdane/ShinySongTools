# recon

Development-only runtime reconnaissance. Reads the live decrypted IL2CPP
metadata cache (the same source the hook resolver uses) and the mapped game
image to answer reverse-engineering questions that offline analysis cannot
(`global-metadata.dat` is encrypted).

Debug topics (require `debug.enabled` and this plugin enabled):

```toml
[recon]
enabled = true
```

- `recon.class {assembly, class}` — method surface (name, param count,
  static flag, VA/RVA of the compiled function), field surface (name, type,
  offset), and raw static field storage words for singleton discovery.
- `recon.callers {rva, limit?}` — direct `bl` call sites of a function RVA
  inside the UnityFramework `__TEXT` section; establishes whether an entry
  point is actually invoked by AOT code.
