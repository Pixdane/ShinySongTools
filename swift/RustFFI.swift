import Foundation

// The single formal Rust entry (runtime crate Rustdoc, "Swift FFI 入口"). Swift keeps no Rust
// state and declares no experiment-only interfaces. The symbol is provided
// by the statically linked Rust runtime (libshiny_song_tools.a).
@_silgen_name("scsp_start")
func scsp_start(_ documentsPath: UnsafePointer<CChar>)
