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
/// `dependsOn` only expresses ordering. Only each pipeline's publish step
/// writes its configured candidate root.
pub fn build(b: *std.Build) void {
    // Build helpers, compiled for the host and run by the graph.
    const tools = BundleTools{
        .patch = hostTool(b, "patch-apply", "tools/zig/patch_apply.zig"),
        .assemble = hostTool(b, "assemble-bundle", "tools/zig/assemble_bundle.zig"),
        .manifest = hostTool(b, "bundle-manifest", "tools/zig/bundle_manifest.zig"),
        .publish = hostTool(b, "bundle-publish", "tools/zig/bundle_publish.zig"),
    };

    addBundlePipeline(b, tools, .{
        .step_name = "bundle",
        .description = "Build, sign, verify and publish AKInterface.bundle",
        .cargo_features = "",
        .cargo_target_dir = "build/target",
        .staticlib_path = "build/target/aarch64-apple-darwin/release/libshiny_song_tools.a",
        .publish_root = "build",
    });

    // Diagnostic-only build. Its feature replaces the production bootstrap
    // body with one delayed domain_get probe and publishes under the
    // experiment output root, never over the normal candidate.
    addBundlePipeline(b, tools, .{
        .step_name = "bootstrap-timing-probe",
        .description = "Build the diagnostic delayed-domain_get bundle",
        .cargo_features = "bootstrap-timing-probe",
        .cargo_target_dir = "build/experiments/bootstrap-timing-probe/target",
        .staticlib_path = "build/experiments/bootstrap-timing-probe/target/aarch64-apple-darwin/release/libshiny_song_tools.a",
        .publish_root = "build/experiments/bootstrap-timing-probe",
    });
}

const BundleTools = struct {
    patch: *std.Build.Step.Compile,
    assemble: *std.Build.Step.Compile,
    manifest: *std.Build.Step.Compile,
    publish: *std.Build.Step.Compile,
};

const BundleOptions = struct {
    step_name: []const u8,
    description: []const u8,
    cargo_features: []const u8,
    cargo_target_dir: []const u8,
    staticlib_path: []const u8,
    publish_root: []const u8,
};

fn addBundlePipeline(b: *std.Build, tools: BundleTools, options: BundleOptions) void {
    const bundle_step = b.step(options.step_name, options.description);

    // 1. Generate the patched Swift bundle source.
    const patch_run = b.addRunArtifact(tools.patch);
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
    //    The normal candidate ships the runtime-config-gated DebugPlugin; only
    //    diagnostic pipelines use a Cargo feature.
    const cargo_run = b.addSystemCommand(&.{
        "cargo",
        "build",
        "--release",
        "--target",
        "aarch64-apple-darwin",
        "--target-dir",
        options.cargo_target_dir,
        "-p",
        "runtime",
    });
    cargo_run.setEnvironmentVariable("MACOSX_DEPLOYMENT_TARGET", "12.0");
    cargo_run.setEnvironmentVariable("RUSTFLAGS", "-C link-arg=-mmacosx-version-min=12.0");
    if (options.cargo_features.len != 0) {
        cargo_run.addArgs(&.{ "--features", options.cargo_features });
    }
    const staticlib = b.path(options.staticlib_path);

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
    const assemble_run = b.addRunArtifact(tools.assemble);
    assemble_run.addArgs(&.{"--exe"});
    assemble_run.addFileArg(bundle_exe);
    assemble_run.addArgs(&.{"--plist"});
    assemble_run.addFileArg(b.path("resources/bundle/Info.plist"));
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
    const swiftc_version_run = b.addSystemCommand(&.{
        "/bin/sh",
        "-c",
        "exec /usr/bin/xcrun swiftc --version 2>/dev/null",
    });
    withoutNixAppleSdkEnv(swiftc_version_run);
    const swiftc_version = swiftc_version_run.captureStdOut(.{ .trim_whitespace = .all });
    const zig_version = captureVersion(b, &.{ "zig", "version" });
    const sdk_version = captureHostSdkVersion(b);

    const manifest_run = b.addRunArtifact(tools.manifest);
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

    // 7. Publish: bundle first, sidecar last, both atomically into the
    //    pipeline's candidate root.
    const publish_run = b.addRunArtifact(tools.publish);
    publish_run.addArgs(&.{"--bundle"});
    publish_run.addDirectoryArg(signed_bundle);
    publish_run.addArgs(&.{"--manifest"});
    publish_run.addFileArg(manifest_file);
    publish_run.addArgs(&.{ "--out-root", b.pathFromRoot(options.publish_root) });
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
