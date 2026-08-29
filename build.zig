const std = @import("std");

/// Complete bundle build graph (docs/bundle-build.md):
///
///   patch (AKPlugin.swift + versioned patch -> patched Swift source)
///     -> cargo (release staticlib for aarch64-apple-darwin, every build)
///     -> xcrun swiftc link (Plugin.swift + patched AKPlugin + RustFFI.swift
///        + staticlib + AppKit/Foundation, deployment target 12.0)
///     -> assemble unsigned bundle (Contents/{Info.plist, MacOS, Resources})
///     -> copy + ad-hoc sign + strict verify
///     -> BundleFingerprintV1 sidecar manifest
///     -> atomic publish to build/AKInterface.bundle (+ sidecar last)
///
/// Graph conventions: sources enter via `b.path` / `addFileArg` /
/// `addDirectoryArg`; generated files leave via `addOutputFileArg` /
/// `addOutputDirectoryArg`; LazyPaths are passed directly between steps;
/// `dependsOn` only expresses ordering. Only the publish step writes
/// `build/AKInterface.bundle`.
pub fn build(b: *std.Build) void {
    const bundle_step = b.step("bundle", "Build, sign, verify and publish AKInterface.bundle");

    // Build helpers, compiled for the host and run by the graph.
    const patch_tool = hostTool(b, "patch-apply", "tools/zig/patch_apply.zig");
    const assemble_tool = hostTool(b, "assemble-bundle", "tools/zig/assemble_bundle.zig");
    const manifest_tool = hostTool(b, "bundle-manifest", "tools/zig/bundle_manifest.zig");
    const publish_tool = hostTool(b, "bundle-publish", "tools/zig/bundle_publish.zig");

    // 1. Generate the patched Swift bundle source.
    const patch_run = b.addRunArtifact(patch_tool);
    patch_run.addArgs(&.{"--input"});
    patch_run.addFileArg(b.path("third_party/PlayTools/AKPlugin.swift"));
    patch_run.addArgs(&.{"--patch"});
    patch_run.addFileArg(b.path("patches/AKPlugin.patch"));
    patch_run.addArgs(&.{"--output"});
    const patched_akplugin = patch_run.addOutputFileArg("AKPlugin.patched.swift");

    // 2. Compile the Rust staticlib. Cargo handles its own incremental
    //    state; this command executes on every entry into the graph. The
    //    staticlib path is declared as a file input of the link step so the
    //    link output is invalidated when the staticlib content changes.
    //    The `debug` feature ships the DebugPlugin capability in the bundle
    //    (v1 deliverable, docs/runtime-architecture.md): whether the debug
    //    plane actually starts stays runtime-config-gated
    //    (`debug.enabled`, fail-closed), so a disabled config behaves
    //    identically to a build without the feature.
    const cargo_run = b.addSystemCommand(&.{
        "cargo",      "build", "--release", "--target", "aarch64-apple-darwin",
        "--features", "debug",
    });
    const staticlib = b.path("build/target/aarch64-apple-darwin/release/libshiny_song_tools.a");

    // 3. Link the bundle executable. The devshell exports DEVELOPER_DIR /
    //    SDKROOT pointing at a Nix Apple SDK that the host Apple Swift
    //    toolchain rejects; strip both (see the link step below).
    const link_run = b.addSystemCommand(&.{
        "/usr/bin/xcrun",        "-sdk",          "macosx",
        "swiftc",                "-O",            "-parse-as-library",
        "-module-name",          "AKInterface",   "-target",
        "arm64-apple-macos12.0", "-emit-library", "-Xlinker",
        "-bundle",               "-framework",    "AppKit",
        "-framework",            "Foundation",
    });
    link_run.addFileArg(b.path("third_party/PlayTools/Plugin.swift"));
    link_run.addFileArg(patched_akplugin);
    link_run.addFileArg(b.path("swift/RustFFI.swift"));
    link_run.addFileArg(staticlib);
    // Pin the host Xcode developer dir: the devshell exports DEVELOPER_DIR
    // / SDKROOT pointing at a Nix Apple SDK that the host Swift toolchain
    // cannot consume.
    // Remove the devshell's Nix Apple SDK env entirely: xcrun then resolves
    // the host toolchain via `xcode-select` (portable across machines).
    link_run.removeEnvironmentVariable("DEVELOPER_DIR");
    link_run.removeEnvironmentVariable("SDKROOT");
    link_run.step.dependOn(&cargo_run.step);
    link_run.addArgs(&.{"-o"});
    const bundle_exe = link_run.addOutputFileArg("AKInterface");

    // 4. Assemble the unsigned bundle.
    const assemble_run = b.addRunArtifact(assemble_tool);
    assemble_run.addArgs(&.{"--exe"});
    assemble_run.addFileArg(bundle_exe);
    assemble_run.addArgs(&.{"--plist"});
    assemble_run.addFileArg(b.path("bundle/Info.plist"));
    assemble_run.addArgs(&.{"--out-dir"});
    const unsigned_bundle = assemble_run.addOutputDirectoryArg("AKInterface.bundle");

    // 5. Sign a fresh copy, then verify strictly. The upstream Zig-cache
    //    inputs are never signed in place.
    const copy_run = b.addSystemCommand(&.{"/usr/bin/ditto"});
    copy_run.addDirectoryArg(unsigned_bundle);
    const signed_bundle = copy_run.addOutputDirectoryArg("AKInterface.bundle.signed");

    const sign_run = b.addSystemCommand(&.{ "/usr/bin/codesign", "-f", "-s", "-", "--timestamp=none" });
    sign_run.addDirectoryArg(signed_bundle);
    sign_run.step.dependOn(&copy_run.step);

    const verify_run = b.addSystemCommand(&.{ "/usr/bin/codesign", "--verify", "--strict" });
    verify_run.addDirectoryArg(signed_bundle);
    verify_run.step.dependOn(&sign_run.step);

    // 6. Sidecar manifest from the verified bundle only.
    const commit_run = b.addSystemCommand(&.{ "git", "-C", "third_party/PlayTools", "rev-parse", "HEAD" });
    const commit_file = commit_run.captureStdOut(.{ .trim_whitespace = .all });
    const rustc_version = captureVersion(b, &.{ "rustc", "--version" });
    const swiftc_version_run = b.addSystemCommand(&.{ "/usr/bin/xcrun", "swiftc", "--version" });
    withoutNixAppleSdkEnv(swiftc_version_run);
    const swiftc_version = swiftc_version_run.captureStdOut(.{ .trim_whitespace = .all });
    const zig_version = captureVersion(b, &.{ "zig", "version" });
    const sdk_version = captureHostSdkVersion(b);

    const manifest_run = b.addRunArtifact(manifest_tool);
    manifest_run.addArgs(&.{"--bundle"});
    manifest_run.addDirectoryArg(signed_bundle);
    manifest_run.addArgs(&.{"--out"});
    const manifest_file = manifest_run.addOutputFileArg("AKInterface.bundle.manifest.json");
    manifest_run.addArgs(&.{"--patch"});
    manifest_run.addFileArg(b.path("patches/AKPlugin.patch"));
    manifest_run.addArgs(&.{"--playtools-commit-file"});
    manifest_run.addFileArg(commit_file);
    manifest_run.addArgs(&.{"--rustc"});
    manifest_run.addFileArg(rustc_version);
    manifest_run.addArgs(&.{"--swiftc"});
    manifest_run.addFileArg(swiftc_version);
    manifest_run.addArgs(&.{"--zig"});
    manifest_run.addFileArg(zig_version);
    manifest_run.addArgs(&.{"--sdk"});
    manifest_run.addFileArg(sdk_version);
    manifest_run.step.dependOn(&verify_run.step);

    // 7. Publish: bundle first, sidecar last, both atomically into build/.
    const publish_run = b.addRunArtifact(publish_tool);
    publish_run.addArgs(&.{"--bundle"});
    publish_run.addDirectoryArg(signed_bundle);
    publish_run.addArgs(&.{"--manifest"});
    publish_run.addFileArg(manifest_file);
    publish_run.addArgs(&.{ "--out-root", b.pathFromRoot("build") });
    publish_run.step.dependOn(&verify_run.step);

    bundle_step.dependOn(&manifest_run.step);
    bundle_step.dependOn(&publish_run.step);
}

fn hostTool(b: *std.Build, name: []const u8, root_source: []const u8) *std.Build.Step.Compile {
    return b.addExecutable(.{
        .name = name,
        .root_module = b.createModule(.{
            .root_source_file = b.path(root_source),
            .target = b.graph.host,
        }),
    });
}

fn captureVersion(b: *std.Build, argv: []const []const u8) std.Build.LazyPath {
    const run = b.addSystemCommand(argv);
    return run.captureStdOut(.{ .trim_whitespace = .all });
}

fn withoutNixAppleSdkEnv(run: *std.Build.Step.Run) void {
    // Remove the devshell's Nix Apple SDK env; xcrun then resolves the host
    // toolchain via `xcode-select` (portable across machines).
    run.removeEnvironmentVariable("DEVELOPER_DIR");
    run.removeEnvironmentVariable("SDKROOT");
}

/// SDK version of the host toolchain (the one the link step actually uses),
/// not the devshell-exported Nix Apple SDK.
fn captureHostSdkVersion(b: *std.Build) std.Build.LazyPath {
    const run = b.addSystemCommand(&.{ "/usr/bin/xcrun", "--show-sdk-version" });
    withoutNixAppleSdkEnv(run);
    return run.captureStdOut(.{ .trim_whitespace = .all });
}
