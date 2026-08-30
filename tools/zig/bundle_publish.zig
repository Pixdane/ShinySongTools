//! Build tool: publish the verified candidate to `build/`.
//!
//! Usage:
//!   bundle-publish --bundle <signed bundle dir> --manifest <manifest.json>
//!                  --out-root <build dir> [--verify-tool <bundle-manifest exe>]
//!
//! Transaction (docs/bundle-build.md step 7):
//!   1. copy the signed bundle into a temporary sibling of the final path;
//!   2. recompute the fingerprint of the copy and compare with the manifest
//!      entries (both copies must match the same content);
//!   3. move any existing final bundle aside, rename the copy into place;
//!   4. write the sidecar manifest to a temp file and rename it over the
//!      final sidecar LAST (bundle first, sidecar last);
//!   5. remove the aside copy.
//!
//! Only this step ever writes `build/AKInterface.bundle` and its sidecar.

const std = @import("std");

fn die(comptime fmt: []const u8, args: anytype) noreturn {
    std.debug.print("bundle-publish: " ++ fmt ++ "\n", args);
    std.process.exit(1);
}

pub fn main(init: std.process.Init) !void {
    const io = init.io;
    const gpa = init.arena.allocator();
    const cwd = std.Io.Dir.cwd();

    var bundle: ?[]const u8 = null;
    var manifest: ?[]const u8 = null;
    var out_root: ?[]const u8 = null;

    var out_dir: std.Io.Dir = undefined;
    var it = init.minimal.args.iterate();
    _ = it.next();
    while (it.next()) |arg| {
        if (std.mem.eql(u8, arg, "--bundle")) {
            bundle = it.next() orelse die("--bundle requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--manifest")) {
            manifest = it.next() orelse die("--manifest requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--out-root")) {
            out_root = it.next() orelse die("--out-root requires a value", .{});
        } else {
            die("unknown argument: {s}", .{arg});
        }
    }
    const bundle_path = bundle orelse die("--bundle is required", .{});
    const manifest_path = manifest orelse die("--manifest is required", .{});
    const root_path = out_root orelse die("--out-root is required", .{});

    var arena_state = std.heap.ArenaAllocator.init(gpa);
    defer arena_state.deinit();
    const arena = arena_state.allocator();

    const final_bundle = try std.fs.path.join(arena, &.{ root_path, "AKInterface.bundle" });
    const final_manifest = try std.fs.path.join(arena, &.{ root_path, "AKInterface.bundle.manifest.json" });
    // Deterministic transaction names: any leftover from an interrupted run
    // is removed up front (interrupted states are invalid candidates by
    // design, never half-trusted ones).
    const stage_bundle = try std.fmt.allocPrint(arena, "{s}.stage", .{final_bundle});
    const old_bundle = try std.fmt.allocPrint(arena, "{s}.old", .{final_bundle});
    const stage_manifest = try std.fmt.allocPrint(arena, "{s}.stage", .{final_manifest});

    // 0. Create and open the output root (a directory handle so cleanup and
    //    renames operate on output-root-relative names). Experiment pipelines
    //    publish below build/experiments/ and may be the first writer there.
    cwd.createDirPath(io, root_path) catch |err|
        die("cannot create output root {s}: {t}", .{ root_path, err });
    out_dir = std.Io.Dir.cwd().openDir(io, root_path, .{}) catch |err|
        die("cannot open output root {s}: {t}", .{ root_path, err });
    defer out_dir.close(io);
    deleteStale(out_dir, io, stage_bundle);
    deleteStale(out_dir, io, old_bundle);
    deleteStale(out_dir, io, stage_manifest);

    // 1. Copy the signed bundle into a temporary sibling path.
    copyDir(arena, io, bundle_path, stage_bundle) catch |err|
        die("copy to stage failed: {t}", .{err});

    // 2. Manifest content must match the staged copy (both came from the
    //    same signed input; a mismatch means the copy was corrupted).
    const manifest_bytes = cwd.readFileAlloc(io, manifest_path, gpa, .limited(16 * 1024 * 1024)) catch |err|
        die("cannot read manifest: {t}", .{err});
    try verifyEntries(arena, io, stage_bundle, manifest_bytes);

    // 3. Bundle first: move the old one aside, rename stage into place.
    //    The move-aside is tolerant of "no previous publish" (any rename
    //    absence is expected; every other error fails loudly).
    var old_moved = false;
    if (cwd.rename(final_bundle, out_dir, old_bundle, io)) |_| {
        old_moved = true;
    } else |err| {
        switch (err) {
            error.FileNotFound => {},
            else => die("cannot move aside the previous bundle: {t}", .{err}),
        }
    }
    cwd.rename(stage_bundle, out_dir, final_bundle, io) catch |err|
        die("cannot publish the staged bundle: {t}", .{err});

    // 4. Sidecar last: write to a temp name, rename over the final path
    //    (file rename is atomic on the same volume).
    const manifest_out = cwd.createFile(io, stage_manifest, .{ .truncate = true }) catch |err|
        die("cannot create staged manifest: {t}", .{err});
    {
        defer manifest_out.close(io);
        manifest_out.writeStreamingAll(io, manifest_bytes) catch |err|
            die("cannot write staged manifest: {t}", .{err});
    }
    cwd.rename(stage_manifest, out_dir, final_manifest, io) catch |err|
        die("cannot publish the staged manifest: {t}", .{err});

    // 5. Remove the aside copy.
    if (old_moved) {
        out_dir.deleteTree(io, std.fs.path.basename(old_bundle)) catch |err|
            die("cannot remove the old bundle copy: {t}", .{err});
    }

    try printStdout(io, gpa, "bundle-publish: {s} + {s}\n", .{ final_bundle, final_manifest });
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

/// Delete a stale transaction path if it exists; ignore absence.
fn deleteStale(dir: std.Io.Dir, io: std.Io, path: []const u8) void {
    dir.deleteTree(io, std.fs.path.basename(path)) catch |err|
        die("cannot clean stale transaction path {s}: {t}", .{ path, err });
}

fn copyDir(arena: std.mem.Allocator, io: std.Io, src: []const u8, dst: []const u8) !void {
    const cwd = std.Io.Dir.cwd();
    try cwd.createDirPath(io, dst);
    const src_dir = try cwd.openDir(io, src, .{});
    defer src_dir.close(io);
    var it = src_dir.iterate();
    while (try it.next(io)) |entry| {
        const child_src = try std.fs.path.join(arena, &.{ src, entry.name });
        const child_dst = try std.fs.path.join(arena, &.{ dst, entry.name });
        switch (entry.kind) {
            .directory => try copyDir(arena, io, child_src, child_dst),
            .file => {
                const bytes = cwd.readFileAlloc(io, child_src, arena, .limited(512 * 1024 * 1024)) catch |err| switch (err) {
                    error.IsDir => {
                        try copyDir(arena, io, child_src, child_dst);
                        continue;
                    },
                    else => return err,
                };
                try cwd.writeFile(io, .{ .sub_path = child_dst, .data = bytes });
                try copyMode(io, child_src, child_dst);
            },
            else => return error.UnsupportedEntry,
        }
    }
}

fn copyMode(io: std.Io, src: []const u8, dst: []const u8) !void {
    const cwd = std.Io.Dir.cwd();
    const src_file = try cwd.openFile(io, src, .{});
    defer src_file.close(io);
    const st = try src_file.stat(io);
    const dst_file = try cwd.openFile(io, dst, .{ .mode = .read_write });
    defer dst_file.close(io);
    try dst_file.setPermissions(io, st.permissions);
}

/// Recompute the BundleFingerprintV1 entries of `bundle_dir` and require an
/// exact structural match with the manifest's entries array.
fn verifyEntries(arena: std.mem.Allocator, io: std.Io, bundle_dir: []const u8, manifest_bytes: []const u8) !void {
    var computed: std.ArrayListUnmanaged(Entry) = .empty;
    const root = try std.Io.Dir.cwd().openDir(io, bundle_dir, .{});
    defer root.close(io);
    try walkEntries(arena, io, root, "", &computed);
    std.mem.sort(Entry, computed.items, {}, lessThanEntry);

    // Parse only the "entries": [ ... ] array of the manifest; each entry is
    // {"path": "...", "sha256": "..."}.
    var recorded: std.ArrayListUnmanaged(Entry) = .empty;
    {
        const entries_key = "\"entries\": [";
        const at = std.mem.indexOf(u8, manifest_bytes, entries_key) orelse
            die("manifest has no entries array", .{});
        var rest = manifest_bytes[at + entries_key.len ..];
        while (std.mem.indexOf(u8, rest, "{\"path\":")) |start| {
            const obj_end = std.mem.indexOf(u8, rest[start..], "}") orelse
                die("manifest entries array is malformed", .{});
            const obj = rest[start .. start + obj_end];
            const path_field = std.mem.indexOf(u8, obj, "\", \"sha256\": \"") orelse
                die("manifest entry is malformed", .{});
            const path = obj[10..path_field];
            const sha = obj[path_field + 14 .. obj.len - 1];
            try recorded.append(arena, .{
                .path = try arena.dupe(u8, path),
                .sha = try arena.dupe(u8, sha),
            });
            rest = rest[start + obj_end ..];
        }
    }
    if (recorded.items.len != computed.items.len)
        die("fingerprint mismatch: {d} recorded vs {d} computed entries", .{ recorded.items.len, computed.items.len });
    for (recorded.items, computed.items) |record, actual| {
        if (!std.mem.eql(u8, record.path, actual.path))
            die("fingerprint mismatch: recorded '{s}' vs computed '{s}'", .{ record.path, actual.path });
        if (!std.mem.eql(u8, record.sha, actual.sha))
            die("fingerprint mismatch for {s}", .{actual.path});
    }
}

const Entry = struct {
    path: []const u8,
    sha: []const u8,
};

fn lessThanEntry(_: void, a: Entry, b: Entry) bool {
    return std.mem.order(u8, a.path, b.path) == .lt;
}

fn walkEntries(
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
                defer sub.close(io);
                try walkEntries(arena, io, sub, child, entries);
            },
            .file => {
                const bytes = dir.readFileAlloc(io, entry.name, arena, .limited(512 * 1024 * 1024)) catch |err|
                    die("cannot read {s}: {t}", .{ child, err });
                var digest: [32]u8 = undefined;
                std.crypto.hash.sha2.Sha256.hash(bytes, &digest, .{});
                const hex = try arena.dupe(u8, &std.fmt.bytesToHex(digest, .lower));
                try entries.append(arena, .{ .path = child, .sha = hex });
            },
            else => die("bundle contains a non-regular entry: {s}", .{child}),
        }
    }
}
