//// A tiny geometry module used as the Gleam tags-query fixture.

import gleam/float
import gleam/io
import gleam/list.{filter, map}

/// Ratio of a circle's circumference to its diameter.
pub const pi: Float = 3.141592653589793

const default_scale = 2.0

/// A named point in the plane.
pub type Point {
  Point(x: Float, y: Float)
}

/// The shapes this module knows how to measure.
pub type Shape {
  Circle(radius: Float)
  Rectangle(width: Float, height: Float)
}

pub type Radius =
  Float

pub opaque type Canvas {
  Canvas(shapes: List(Shape))
}

/// Area of a single shape.
pub fn area(shape: Shape) -> Float {
  case shape {
    Circle(radius) -> pi *. radius *. radius
    Rectangle(width, height) -> width *. height
  }
}

fn scale(shape: Shape, factor: Float) -> Shape {
  case shape {
    Circle(radius) -> Circle(radius *. factor)
    Rectangle(width, height) -> Rectangle(width *. factor, height *. factor)
  }
}

pub fn total_area(shapes: List(Shape)) -> Float {
  shapes
  |> map(area)
  |> float.sum
}

pub fn report(canvas: Canvas) -> Nil {
  let Canvas(shapes) = canvas
  let big = filter(shapes, fn(shape) { area(shape) >. 1.0 })
  io.println(float.to_string(total_area(big)))
  let doubled = scale(Circle(1.0), default_scale)
  io.debug(area(doubled))
}
