/// A tiny geometry library used as the Dart tags-query fixture.
library geometry;

import 'dart:math' as math;
import 'package:collection/collection.dart' show IterableExtension;

const double defaultScale = 2.0;

/// Anything that can report an area.
abstract class Shape {
  double get area;

  String describe();
}

mixin Loggable {
  final List<String> log = <String>[];

  void record(String message) {
    log.add(message);
  }
}

class Circle extends Shape with Loggable implements Comparable<Circle> {
  Circle(this.radius);

  factory Circle.unit() => Circle(1.0);

  final double radius;

  static const double tau = 2 * math.pi;

  @override
  double get area => math.pi * radius * radius;

  @override
  String describe() {
    record('circle');
    return 'circle of area ${area.toStringAsFixed(2)}';
  }

  @override
  int compareTo(Circle other) => area.compareTo(other.area);

  Circle operator +(Circle other) => Circle(radius + other.radius);
}

enum Palette { red, green, blue }

typedef Transform = double Function(double value);

extension ShapeList on List<Shape> {
  double get total => fold(0.0, (sum, shape) => sum + shape.area);
}

double scaleAll(List<Shape> shapes, Transform transform) {
  var sum = 0.0;
  for (final shape in shapes) {
    sum += transform(shape.area);
  }
  return sum;
}

void main() {
  final shapes = <Shape>[Circle(1.5), Circle.unit()];
  final scaled = scaleAll(shapes, (value) => value * defaultScale);
  print(shapes.first.describe());
  print('${shapes.total} $scaled ${Palette.red}');
}
