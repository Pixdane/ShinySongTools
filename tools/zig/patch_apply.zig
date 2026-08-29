//! Build tool: apply the versioned AKPlugin patch to the upstream source.
//!
//! Usage:
//!   patch-apply --input <AKPlugin.swift> --patch <patches/AKPlugin.patch> --output <file>
//!
//! Format: sequential context-verified replacement blocks (`<<< BEFORE` /
//! `<<< AFTER` / `<<< END`). Each BEFORE block must occur exactly once in
//! the (already patched) text; any other count is context drift and fails
//! the build before anything is written.

const std = @import("std");

const Block = struct {
    before: []const u8,
    after: []const u8,
};

fn die(comptime fmt: []const u8, args: anytype) noreturn {
    std.debug.print("patch-apply: " ++ fmt ++ "\n", args);
    std.process.exit(1);
}

pub fn main(init: std.process.Init) !void {
    const io = init.io;
    const gpa = init.arena.allocator();
    const cwd = std.Io.Dir.cwd();

    var input: ?[]const u8 = null;
    var patch: ?[]const u8 = null;
    var output: ?[]const u8 = null;

    var it = init.minimal.args.iterate();
    _ = it.next();
    while (it.next()) |arg| {
        if (std.mem.eql(u8, arg, "--input")) {
            input = it.next() orelse die("--input requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--patch")) {
            patch = it.next() orelse die("--patch requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--output")) {
            output = it.next() orelse die("--output requires a value", .{});
        } else {
            die("unknown argument: {s}", .{arg});
        }
    }
    const in_path = input orelse die("--input is required", .{});
    const patch_path = patch orelse die("--patch is required", .{});
    const out_path = output orelse die("--output is required", .{});

    const source = cwd.readFileAlloc(io, in_path, gpa, .limited(64 * 1024 * 1024)) catch |err|
        die("cannot read {s}: {t}", .{ in_path, err });
    const patch_text = cwd.readFileAlloc(io, patch_path, gpa, .limited(4 * 1024 * 1024)) catch |err|
        die("cannot read {s}: {t}", .{ patch_path, err });

    const blocks = parseBlocks(gpa, patch_text) catch |err| switch (err) {
        error.NoBeforeMarker => die("patch file: BEFORE marker without content", .{}),
        error.NoAfterMarker => die("patch file: BEFORE block without AFTER marker", .{}),
        error.NoEndMarker => die("patch file: AFTER block without END marker", .{}),
        error.EmptyBefore => die("patch file: empty BEFORE block", .{}),
        error.OutOfMemory => die("out of memory", .{}),
    };
    if (blocks.len == 0) die("patch file contains no blocks", .{});

    // Sequential application with exactly-once context verification.
    var text: []u8 = source;
    for (blocks, 1..) |block, index| {
        const count = countOccurrences(text, block.before);
        if (count != 1) {
            die(
                "patch block #{d}: context drift (found {d} occurrences, expected exactly 1)",
                .{ index, count },
            );
        }
        text = try replaceOne(gpa, text, block.before, block.after);
    }

    cwd.writeFile(io, .{
        .sub_path = out_path,
        .data = text,
    }) catch |err| die("cannot write {s}: {t}", .{ out_path, err });
    std.debug.print(
        "patch-apply: applied {d} block(s): {s} -> {s}\n",
        .{ blocks.len, in_path, out_path },
    );
}

fn parseBlocks(gpa: std.mem.Allocator, patch_text: []const u8) ![]Block {
    var blocks: std.ArrayListUnmanaged(Block) = .empty;
    var lines = std.mem.splitSequence(u8, patch_text, "\n");
    var before_buf: std.ArrayListUnmanaged(u8) = .empty;
    var after_buf: std.ArrayListUnmanaged(u8) = .empty;
    var state: enum { idle, before, after } = .idle;

    while (lines.next()) |line| {
        const trimmed = std.mem.trimEnd(u8, line, "\r");
        if (state == .idle and (trimmed.len == 0 or trimmed[0] == '#')) continue;
        if (std.mem.eql(u8, trimmed, "<<< BEFORE")) {
            if (state != .idle) return error.NoEndMarker;
            state = .before;
            before_buf = .empty;
        } else if (std.mem.eql(u8, trimmed, "<<< AFTER")) {
            if (state != .before) return error.NoBeforeMarker;
            state = .after;
            after_buf = .empty;
        } else if (std.mem.eql(u8, trimmed, "<<< END")) {
            if (state != .after) return error.NoAfterMarker;
            if (before_buf.items.len == 0) return error.EmptyBefore;
            try blocks.append(gpa, .{
                .before = before_buf.items,
                .after = after_buf.items,
            });
            state = .idle;
        } else switch (state) {
            .idle => {},
            .before => {
                try before_buf.appendSlice(gpa, trimmed);
                try before_buf.appendSlice(gpa, "\n");
            },
            .after => {
                try after_buf.appendSlice(gpa, trimmed);
                try after_buf.appendSlice(gpa, "\n");
            },
        }
    }
    if (state != .idle) return error.NoEndMarker;
    return blocks.items;
}

fn countOccurrences(haystack: []const u8, needle: []const u8) usize {
    var count: usize = 0;
    var rest = haystack;
    while (std.mem.indexOf(u8, rest, needle)) |at| {
        count += 1;
        rest = rest[at + needle.len ..];
    }
    return count;
}

fn replaceOne(
    gpa: std.mem.Allocator,
    haystack: []const u8,
    needle: []const u8,
    replacement: []const u8,
) ![]u8 {
    const at = std.mem.indexOf(u8, haystack, needle).?;
    var out: std.ArrayListUnmanaged(u8) = .empty;
    try out.appendSlice(gpa, haystack[0..at]);
    try out.appendSlice(gpa, replacement);
    try out.appendSlice(gpa, haystack[at + needle.len ..]);
    return out.items;
}
