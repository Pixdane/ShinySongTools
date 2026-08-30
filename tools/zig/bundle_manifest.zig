//! Build tool: BundleFingerprintV1 manifest generation.
//!
//! Usage:
//!   bundle-manifest --bundle <signed bundle dir> --out <manifest.json>
//!                   --patch <patches/AKPlugin.patch> --patch-path <rel>
//!                   --playtools-commit-file <file> --playtools-repo <url>
//!                   --target <triple> --rustc <v> --swiftc <v>
//!                   --zig <v> --sdk <v>
//!
//! Fingerprint rules (docs/bundle-build.md step 6):
//!  1. walk the final signed bundle; reject symlinks and anything that is
//!     not a directory or regular file;
//!  2. POSIX relative paths from the bundle root; no absolute/empty/`..`;
//!  3. include every regular file (executable, Info.plist, _CodeSignature);
//!  4. entries sorted by UTF-8 byte order with per-file SHA-256;
//!  5. identity = structural equality of the whole ordered entry vector.

const std = @import("std");

fn die(comptime fmt: []const u8, args: anytype) noreturn {
    std.debug.print("bundle-manifest: " ++ fmt ++ "\n", args);
    std.process.exit(1);
}

const Entry = struct {
    path: []const u8,
    sha256: [64]u8,
};

pub fn main(init: std.process.Init) !void {
    const io = init.io;
    const gpa = init.arena.allocator();
    const cwd = std.Io.Dir.cwd();

    var bundle: ?[]const u8 = null;
    var out: ?[]const u8 = null;
    var patch: ?[]const u8 = null;
    var patch_rel: []const u8 = "patches/AKPlugin.patch";
    var commit_file: ?[]const u8 = null;
    var repo: []const u8 = "https://github.com/PlayCover/PlayTools.git";
    var target: []const u8 = "arm64-apple-macos12.0";
    var rustc_v: ?[]const u8 = null;
    var swiftc_v: ?[]const u8 = null;
    var zig_v: ?[]const u8 = null;
    var sdk_v: ?[]const u8 = null;

    var it = init.minimal.args.iterate();
    _ = it.next();
    while (it.next()) |arg| {
        if (std.mem.eql(u8, arg, "--bundle")) {
            bundle = it.next() orelse die("--bundle requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--out")) {
            out = it.next() orelse die("--out requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--patch")) {
            patch = it.next() orelse die("--patch requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--patch-path")) {
            patch_rel = it.next() orelse die("--patch-path requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--playtools-commit-file")) {
            commit_file = it.next() orelse die("--playtools-commit-file requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--playtools-repo")) {
            repo = it.next() orelse die("--playtools-repo requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--target")) {
            target = it.next() orelse die("--target requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--rustc")) {
            rustc_v = it.next() orelse die("--rustc requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--swiftc")) {
            swiftc_v = it.next() orelse die("--swiftc requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--zig")) {
            zig_v = it.next() orelse die("--zig requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--sdk")) {
            sdk_v = it.next() orelse die("--sdk requires a value", .{});
        } else {
            die("unknown argument: {s}", .{arg});
        }
    }
    const bundle_path = bundle orelse die("--bundle is required", .{});
    const out_path = out orelse die("--out is required", .{});
    const patch_path = patch orelse die("--patch is required", .{});
    const commit_path = commit_file orelse die("--playtools-commit-file is required", .{});
    const rustc_path = rustc_v orelse die("--rustc is required", .{});
    const swiftc_path = swiftc_v orelse die("--swiftc is required", .{});
    const zig_path = zig_v orelse die("--zig is required", .{});
    const sdk_path = sdk_v orelse die("--sdk is required", .{});

    var arena_state = std.heap.ArenaAllocator.init(gpa);
    defer arena_state.deinit();
    const arena = arena_state.allocator();

    // Walk the signed bundle.
    var entries: std.ArrayListUnmanaged(Entry) = .empty;
    const bundle_root = cwd.openDir(io, bundle_path, .{}) catch |err|
        die("cannot open bundle dir {s}: {t}", .{ bundle_path, err });
    try walk(arena, io, bundle_root, "", &entries);
    if (entries.items.len == 0) die("bundle contains no files", .{});

    // Sort by UTF-8 byte order of the relative path.
    std.mem.sort(Entry, entries.items, {}, struct {
        fn lessThan(_: void, a: Entry, b: Entry) bool {
            return std.mem.order(u8, a.path, b.path) == .lt;
        }
    }.lessThan);

    // Required members.
    const exec_sha = findSha(&entries, "Contents/MacOS/AKInterface");
    const plist_sha = findSha(&entries, "Contents/Info.plist");

    const patch_bytes = cwd.readFileAlloc(io, patch_path, gpa, .limited(4 * 1024 * 1024)) catch |err|
        die("cannot read patch: {t}", .{err});
    const patch_sha = sha256Hex(patch_bytes);

    const commit_bytes_raw = cwd.readFileAlloc(io, commit_path, gpa, .limited(1024)) catch |err|
        die("cannot read commit file: {t}", .{err});
    const commit = std.mem.trim(u8, commit_bytes_raw, " \n\r\t");
    if (commit.len != 40) die("PlayTools commit must be a 40-char hash, got '{s}'", .{commit});

    const rustc_version = try readCaptured(arena, io, rustc_path, "rustc version");
    const swiftc_version = try readCaptured(arena, io, swiftc_path, "swiftc version");
    const zig_version = try readCaptured(arena, io, zig_path, "zig version");
    const sdk_version = try readCaptured(arena, io, sdk_path, "sdk version");

    // Compose JSON.
    var json: std.ArrayListUnmanaged(u8) = .empty;
    const w = &json;
    try w.appendSlice(arena, "{\n");
    try w.appendSlice(arena, "  \"schema_version\": 1,\n");
    try w.appendSlice(arena, "  \"bundle\": {\n");
    try w.appendSlice(arena, "    \"executable\": \"AKInterface\",\n");
    try printField(w, arena, "    \"executable_sha256\"", exec_sha, false);
    try printField(w, arena, "    \"info_plist_sha256\"", plist_sha, false);
    try printField(w, arena, "    \"target\"", target, true);
    try w.appendSlice(arena, "  },\n");
    try w.appendSlice(arena, "  \"fingerprint\": {\n");
    try w.appendSlice(arena, "    \"version\": \"BundleFingerprintV1\",\n");
    try w.appendSlice(arena, "    \"entries\": [\n");
    for (entries.items, 0..) |entry, index| {
        try w.appendSlice(arena, "      {\"path\": ");
        try jsonString(w, arena, entry.path);
        try w.appendSlice(arena, ", \"sha256\": ");
        try jsonString(w, arena, &entry.sha256);
        try w.appendSlice(arena, "}");
        if (index + 1 != entries.items.len) try w.appendSlice(arena, ",");
        try w.appendSlice(arena, "\n");
    }
    try w.appendSlice(arena, "    ]\n");
    try w.appendSlice(arena, "  },\n");
    try w.appendSlice(arena, "  \"upstream\": {\n");
    try printField(w, arena, "    \"repository\"", repo, false);
    try printField(w, arena, "    \"commit\"", commit, true);
    try w.appendSlice(arena, "  },\n");
    try w.appendSlice(arena, "  \"patch\": {\n");
    try printField(w, arena, "    \"path\"", patch_rel, false);
    try printField(w, arena, "    \"sha256\"", &patch_sha, true);
    try w.appendSlice(arena, "  },\n");
    try w.appendSlice(arena, "  \"platform\": {\n");
    try printField(w, arena, "    \"architecture\"", "arm64", false);
    try printField(w, arena, "    \"deployment_target\"", "12.0", false);
    try printField(w, arena, "    \"signature\"", "ad-hoc", true);
    try w.appendSlice(arena, "  },\n");
    try w.appendSlice(arena, "  \"toolchain\": {\n");
    try printField(w, arena, "    \"rustc\"", rustc_version, false);
    try printField(w, arena, "    \"swiftc\"", swiftc_version, false);
    try printField(w, arena, "    \"zig\"", zig_version, false);
    try printField(w, arena, "    \"sdk\"", sdk_version, true);
    try w.appendSlice(arena, "  }\n");
    try w.appendSlice(arena, "}\n");

    cwd.writeFile(io, .{ .sub_path = out_path, .data = json.items }) catch |err|
        die("cannot write manifest: {t}", .{err});
    try printStdout(io, gpa, "bundle-manifest: {s} ({d} entries)\n", .{ out_path, entries.items.len });
}

fn printStdout(
    io: std.Io,
    allocator: std.mem.Allocator,
    comptime fmt: []const u8,
    args: anytype,
) !void {
    const message = try std.fmt.allocPrint(allocator, fmt, args);
    try std.Io.File.stdout().writeStreamingAll(io, message);
}

fn walk(
    arena: std.mem.Allocator,
    io: std.Io,
    dir: std.Io.Dir,
    prefix: []const u8,
    entries: *std.ArrayListUnmanaged(Entry),
) !void {
    var it = dir.iterate();
    while (try it.next(io)) |entry| {
        const child = try std.fs.path.join(arena, &.{ prefix, entry.name });
        switch (entry.kind) {
            .directory => {
                const sub = try dir.openDir(io, entry.name, .{});
                var sub_closed = false;
                defer if (!sub_closed) sub.close(io);
                try walk(arena, io, sub, child, entries);
                sub_closed = true;
            },
            .file => {
                const bytes = dir.readFileAlloc(io, entry.name, arena, .limited(256 * 1024 * 1024)) catch |err|
                    die("cannot read bundle file {s}: {t}", .{ child, err });
                const hex = sha256Hex(bytes);
                var path_bytes: [std.Io.Dir.max_path_bytes]u8 = undefined;
                if (child.len >= std.Io.Dir.max_path_bytes) die("bundle path too long: {s}", .{child});
                @memcpy(path_bytes[0..child.len], child);
                try entries.append(arena, .{
                    .path = try arena.dupe(u8, child),
                    .sha256 = hex,
                });
            },
            else => die("bundle contains a non-regular entry: {s} (kind {t})", .{ child, entry.kind }),
        }
    }
}

fn readCaptured(arena: std.mem.Allocator, io: std.Io, path: []const u8, label: []const u8) ![]const u8 {
    const bytes = std.Io.Dir.cwd().readFileAlloc(io, path, arena, .limited(4096)) catch |err|
        die("cannot read {s} capture: {t}", .{ label, err });
    return std.mem.trim(u8, bytes, " \n\r\t");
}

fn findSha(entries: *const std.ArrayListUnmanaged(Entry), path: []const u8) []const u8 {
    for (entries.items) |*entry| {
        if (std.mem.eql(u8, entry.path, path)) return &entry.sha256;
    }
    die("bundle is missing required file: {s}", .{path});
}

fn sha256Hex(data: []const u8) [64]u8 {
    var digest: [32]u8 = undefined;
    std.crypto.hash.sha2.Sha256.hash(data, &digest, .{});
    return std.fmt.bytesToHex(digest, .lower);
}

fn printField(
    w: *std.ArrayListUnmanaged(u8),
    arena: std.mem.Allocator,
    key: []const u8,
    value: []const u8,
    last: bool,
) !void {
    try w.appendSlice(arena, key);
    try w.appendSlice(arena, ": ");
    try jsonString(w, arena, value);
    if (!last) try w.appendSlice(arena, ",");
    try w.appendSlice(arena, "\n");
}

fn jsonString(w: *std.ArrayListUnmanaged(u8), arena: std.mem.Allocator, value: []const u8) !void {
    try w.append(arena, '"');
    for (value) |c| {
        switch (c) {
            '"' => try w.appendSlice(arena, "\\\""),
            '\\' => try w.appendSlice(arena, "\\\\"),
            '\n' => try w.appendSlice(arena, "\\n"),
            '\r' => try w.appendSlice(arena, "\\r"),
            '\t' => try w.appendSlice(arena, "\\t"),
            else => {
                if (c < 0x20) {
                    var buf: [8]u8 = undefined;
                    const printed = try std.fmt.bufPrint(&buf, "\\u{x:0>4}", .{c});
                    try w.appendSlice(arena, printed);
                } else {
                    try w.append(arena, c);
                }
            },
        }
    }
    try w.append(arena, '"');
}
