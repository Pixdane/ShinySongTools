//! Build tool: assemble the unsigned AKInterface bundle.
//!
//! Usage:
//!   assemble-bundle --exe <executable> --plist <Info.plist> --out-dir <dir>
//!
//! Layout (docs/bundle-build.md step 4):
//!   <out-dir>/Contents/Info.plist
//!   <out-dir>/Contents/MacOS/AKInterface
//!   <out-dir>/Contents/Resources/

const std = @import("std");

fn die(comptime fmt: []const u8, args: anytype) noreturn {
    std.debug.print("assemble-bundle: " ++ fmt ++ "\n", args);
    std.process.exit(1);
}

pub fn main(init: std.process.Init) !void {
    const io = init.io;
    const gpa = init.arena.allocator();
    const cwd = std.Io.Dir.cwd();

    var exe: ?[]const u8 = null;
    var plist: ?[]const u8 = null;
    var out_dir: ?[]const u8 = null;

    var it = init.minimal.args.iterate();
    _ = it.next();
    while (it.next()) |arg| {
        if (std.mem.eql(u8, arg, "--exe")) {
            exe = it.next() orelse die("--exe requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--plist")) {
            plist = it.next() orelse die("--plist requires a value", .{});
        } else if (std.mem.eql(u8, arg, "--out-dir")) {
            out_dir = it.next() orelse die("--out-dir requires a value", .{});
        } else {
            die("unknown argument: {s}", .{arg});
        }
    }
    const exe_path = exe orelse die("--exe is required", .{});
    const plist_path = plist orelse die("--plist is required", .{});
    const dir_path = out_dir orelse die("--out-dir is required", .{});

    const exe_bytes = cwd.readFileAlloc(io, exe_path, gpa, .limited(256 * 1024 * 1024)) catch |err|
        die("cannot read {s}: {t}", .{ exe_path, err });
    const plist_bytes = cwd.readFileAlloc(io, plist_path, gpa, .limited(4 * 1024 * 1024)) catch |err|
        die("cannot read {s}: {t}", .{ plist_path, err });

    cwd.createDirPath(io, dir_path) catch |err| die("cannot create {s}: {t}", .{ dir_path, err });
    const contents_path = try std.fs.path.join(gpa, &.{ dir_path, "Contents" });
    const macos_path = try std.fs.path.join(gpa, &.{ contents_path, "MacOS" });
    const resources_path = try std.fs.path.join(gpa, &.{ contents_path, "Resources" });
    cwd.createDirPath(io, contents_path) catch |err| die("cannot create Contents: {t}", .{err});
    cwd.createDirPath(io, macos_path) catch |err| die("cannot create MacOS: {t}", .{err});
    cwd.createDirPath(io, resources_path) catch |err| die("cannot create Resources: {t}", .{err});

    const plist_out = try std.fs.path.join(gpa, &.{ contents_path, "Info.plist" });
    const exe_out = try std.fs.path.join(gpa, &.{ macos_path, "AKInterface" });
    cwd.writeFile(io, .{ .sub_path = plist_out, .data = plist_bytes }) catch |err|
        die("cannot write Info.plist: {t}", .{err});
    cwd.writeFile(io, .{ .sub_path = exe_out, .data = exe_bytes }) catch |err|
        die("cannot write executable: {t}", .{err});

    // The bundle executable must be runnable: keep the source mode bits.
    // (`writeFile` creates 0o666 & ~umask; add the executable bit.)
    setExecutableBit(io, exe_out) catch |err|
        die("cannot mark executable: {t}", .{err});

    try printStdout(io, gpa, "assemble-bundle: {s}\n", .{dir_path});
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

fn setExecutableBit(io: std.Io, path: []const u8) !void {
    const cwd = std.Io.Dir.cwd();
    const file = try cwd.openFile(io, path, .{ .mode = .read_write });
    defer file.close(io);
    const st = try file.stat(io);
    const mode = st.permissions.toMode() | 0o111;
    try file.setPermissions(io, @enumFromInt(mode));
}
