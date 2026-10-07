//! A tiny geometry module used as the Zig tags-query fixture.

const std = @import("std");
const math = std.math;

pub const max_shapes: usize = 16;

pub const Error = error{ TooManyShapes, Degenerate };

/// A point in the plane.
pub const Point = struct {
    x: f64,
    y: f64,

    pub fn init(x: f64, y: f64) Point {
        return .{ .x = x, .y = y };
    }

    pub fn magnitude(self: Point) f64 {
        return math.sqrt(self.x * self.x + self.y * self.y);
    }
};

pub const Kind = enum {
    circle,
    rectangle,
};

pub const Shape = union(Kind) {
    circle: f64,
    rectangle: Point,
};

fn area(shape: Shape) f64 {
    return switch (shape) {
        .circle => |radius| math.pi * radius * radius,
        .rectangle => |corner| corner.x * corner.y,
    };
}

pub fn totalArea(shapes: []const Shape) Error!f64 {
    if (shapes.len > max_shapes) return Error.TooManyShapes;
    var total: f64 = 0;
    for (shapes) |shape| {
        total += area(shape);
    }
    return total;
}

pub fn main() !void {
    const origin = Point.init(0, 0);
    std.debug.print("{d}\n", .{origin.magnitude()});
    const shapes = [_]Shape{ .{ .circle = 1.0 }, .{ .rectangle = origin } };
    const total = try totalArea(&shapes);
    std.debug.print("{d}\n", .{total});
}

test "total area sums every shape" {
    const shapes = [_]Shape{.{ .circle = 1.0 }};
    try std.testing.expect(try totalArea(&shapes) > 3.0);
}
