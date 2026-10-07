#![cfg(feature = "index")]

use std::{fs, path::PathBuf};

use tempfile::tempdir;
use tokio_util::sync::CancellationToken;
use workcell_mcp_files::{
    FileResourceAccess, FileToolGroup, FilesystemError, IndexDirectoryEntryKind,
    IndexExecutionConfiguration, IndexInput, IndexLimits, IndexLineSemantic, IndexOutput,
    IndexOutputLine, IndexSourceRange,
};

struct LanguageCase {
    filename: &'static str,
    source: &'static str,
    language: &'static str,
    expected: &'static str,
}

struct NativeConstructCase {
    filename: &'static str,
    source: &'static str,
    required: &'static [&'static str],
}

struct ParityCase {
    filename: &'static str,
    source: &'static str,
    expected: &'static str,
}

const INDEX_TYPE_CHANGED: &str = "Index path type changed after authorization";

const LANGUAGE_CASES: &[LanguageCase] = &[
    LanguageCase {
        filename: "case.rs",
        source: "pub fn run(x: i32) -> i32 { x }\n",
        language: "rust",
        expected: "fns:\n  pub run(x: i32) -> i32 [1]",
    },
    LanguageCase {
        filename: "case.py",
        source: "def run(x: int) -> str:\n    return str(x)\n",
        language: "python",
        expected: "fns:\n  run(x: int) -> str [1-2]",
    },
    LanguageCase {
        filename: "case.ts",
        source: "export function run(x: number): string { return String(x); }\n",
        language: "typescript",
        expected: "fns:\n  export run(x: number): string [1]",
    },
    LanguageCase {
        filename: "case.js",
        source: "export function run(x) { return x; }\n",
        language: "javascript",
        expected: "fns:\n  export run(x) [1]",
    },
    LanguageCase {
        filename: "case.gleam",
        source: "pub fn run(x: Int) -> Int { x }\n",
        language: "gleam",
        expected: "fns:\n  fn run(x: Int) -> Int [1]",
    },
    LanguageCase {
        filename: "case.go",
        source: "package main\nfunc run(x int) int { return x }\n",
        language: "go",
        expected: "fns:\n  run(x int) int [2]",
    },
    LanguageCase {
        filename: "case.html",
        source: "<main><section id=\"content\"></section></main>\n",
        language: "html",
        expected: "structure:\n  <main> [1]\n    <section#content> [1]",
    },
    LanguageCase {
        filename: "case.java",
        source: "public class Service { public void run() {} }\n",
        language: "java",
        expected: "classes:\n  public class Service [1]\n    public void run() [1]",
    },
    LanguageCase {
        filename: "case.c",
        source: "int run(int value);\n",
        language: "c",
        expected: "fns:\n  int run(int value) [1]",
    },
    LanguageCase {
        filename: "case.cpp",
        source: "class Service { public: void run(); };\n",
        language: "cpp",
        expected: "classes:\n  class Service [1]\n    void run() [1]",
    },
    LanguageCase {
        filename: "case.cs",
        source: "public class Service { public void Run() {} }\n",
        language: "c_sharp",
        expected: "classes:\n  public class Service  [1]\n    public void Run() [1]",
    },
    LanguageCase {
        filename: "case.rb",
        source: "class Service\n  def run(value)\n  end\nend\n",
        language: "ruby",
        expected: "classes:\n  Service [1-4]\n    run(value) [2-3]",
    },
    LanguageCase {
        filename: "case.php",
        source: "<?php function run(int $value): int { return $value; }\n",
        language: "php",
        expected: "fns:\n  function run(int $value): int [1]",
    },
    LanguageCase {
        filename: "case.swift",
        source: "public func run(value: Int) -> Int { value }\n",
        language: "swift",
        expected: "fns:\n  public func run() [1]",
    },
    LanguageCase {
        filename: "case.kt",
        source: "fun run(value: Int): Int = value\n",
        language: "kotlin",
        expected: "fns:\n  fun run(value: Int) [1]",
    },
    LanguageCase {
        filename: "case.scala",
        source: "def run(value: Int): Int = value\n",
        language: "scala",
        expected: "fns:\n  def run(value: Int): Int [1]",
    },
    LanguageCase {
        filename: "case.sh",
        source: "run() { echo ok; }\n",
        language: "bash",
        expected: "fns:\n  run() [1]",
    },
    LanguageCase {
        filename: "case.lua",
        source: "function run(value) return value end\n",
        language: "lua_lang",
        expected: "fns:\n  run(value) [1]",
    },
    LanguageCase {
        filename: "case.ex",
        source: "defmodule Service do\n  def run(value), do: value\nend\n",
        language: "elixir",
        expected: "classes:\n  defmodule Service [1-3]\n    def run(value) [2]",
    },
    LanguageCase {
        filename: "case.md",
        source: "# Main\n\n## Detail\n",
        language: "markdown",
        expected: "headings:\n  # Main [1-4]\n  ## Detail [3-4]",
    },
    LanguageCase {
        filename: "BUILD",
        source: "cc_library(name = \"core\", srcs = [\"core.cc\"])\n",
        language: "bazel_build",
        expected: "targets:\n    core: cc_library [1]",
    },
    LanguageCase {
        filename: "MODULE.bazel",
        source: "module(name = \"demo\", version = \"1.0\")\n",
        language: "bazel_module",
        expected: "module:\n  \"@demo\" [1]",
    },
    LanguageCase {
        filename: "case.bzl",
        source: "def run(value):\n    return value\n",
        language: "bazel_bzl",
        expected: "functions:\n  run(value) [1-2]",
    },
    LanguageCase {
        filename: "case.zig",
        source: "pub fn run(value: i32) i32 { return value; }\n",
        language: "zig",
        expected: "fns:\n  run(value: i32) i32 [1]",
    },
    LanguageCase {
        filename: "case.nix",
        source: "{ name = \"demo\"; version = \"1.0\"; }\n",
        language: "nix",
        expected: "consts:\n  name [1]\n  version [1]",
    },
    LanguageCase {
        filename: "case.dart",
        source: "int run(int value) { return value; }\n",
        language: "dart",
        expected: "fns:\n  run(int value) int [1]",
    },
    LanguageCase {
        filename: "case.toml",
        source: "[package]\nname = \"demo\"\n",
        language: "toml",
        expected: "consts:\n  [package] [1-3]\n    name = \"demo\"",
    },
    LanguageCase {
        filename: "case.yaml",
        source: "service:\n  name: demo\n",
        language: "yaml",
        expected: "consts:\n  service [1-3]\n    name [2]",
    },
    LanguageCase {
        filename: "case.sql",
        source: "CREATE TABLE users (id INT);\n",
        language: "sql",
        expected: "classes:\n  TABLE users [1]\n    id INT",
    },
    LanguageCase {
        filename: "case.css",
        source: ".card { color: red; }\n",
        language: "css",
        expected: "rules:\n  .card [1]",
    },
    LanguageCase {
        filename: "case.json",
        source: "{\"name\": \"demo\"}\n",
        language: "json",
        expected: "consts:\n  \"name\" [1]",
    },
    LanguageCase {
        filename: "case.hcl",
        source: "resource \"service\" \"main\" { enabled = true }\n",
        language: "hcl",
        expected: "blocks:\n  resource \"service\" \"main\" [1]\n    enabled [1]",
    },
    LanguageCase {
        filename: "Containerfile",
        source: "FROM scratch\nCOPY app /app\n",
        language: "containerfile",
        expected: "instructions:\n  FROM scratch [1]\n  COPY app /app [2]",
    },
    LanguageCase {
        filename: "Makefile",
        source: "build:\n\tcargo build\n",
        language: "make",
        expected: "targets:\n  build: [1-2]",
    },
    LanguageCase {
        filename: "case.cu",
        source: "__global__ void scale(float* data);\n",
        language: "cuda",
        expected: "fns:\n  __global__ void scale(float* data) [1]",
    },
    LanguageCase {
        filename: "case.m",
        source: "@class Forward;\n@interface Canvas : NSObject\n- (void)draw;\n@end\n",
        language: "objc",
        expected: "classes:\n  @interface Canvas : NSObject [2-4]\n    - (void)draw [3]",
    },
    LanguageCase {
        filename: "CMakeLists.txt",
        source: "project(Demo)\nfunction(build target)\n  set(local 1)\nendfunction()\n",
        language: "cmake",
        expected: "mod: [1]\n  Demo\n\nfns:\n  function(build target) [2-4]",
    },
    LanguageCase {
        filename: "case.proto",
        source: "package demo;\nmessage Item {\n  string id = 1;\n  message Part {}\n}\nenum Kind { KIND_A = 0; KIND_B = 1; }\n",
        language: "proto",
        expected: "mod: [1]\n  demo\n\ntypes:\n  message Item [2-5]\n    string id = 1\n  message Item.Part [4]\n  enum Kind [6]\n    KIND_A, KIND_B",
    },
    LanguageCase {
        filename: "case.xml",
        source: "<?xml version=\"1.0\"?>\n<project>\n  <dependencies>\n    <dependency><id>a</id></dependency>\n    <dependency><id>b</id></dependency>\n  </dependencies>\n  <build><plugins><plugin/></plugins></build>\n</project>\n",
        language: "xml",
        expected: "structure:\n  <project> [2-8]\n    <dependencies> [3-6]\n      <dependency> ×2 [4-5]\n    <build> [7]\n      <plugins> [7]",
    },
];

const NATIVE_CONSTRUCT_CASES: &[NativeConstructCase] = &[
    NativeConstructCase {
        filename: "native.rs",
        source: "use std::io;\nuse std::fs;\npub trait Run { fn run(&self); }\nimpl Run for Service { fn run(&self) {} }\n",
        required: &["imports: [1-2]", "std::{fs, io}", "traits:", "impls:"],
    },
    NativeConstructCase {
        filename: "native.py",
        source: "from pkg import first, second\nVALUE = 3\nclass Service:\n    def run(self, value: int) -> str:\n        return str(value)\n",
        required: &[
            "pkg.{first, second}",
            "VALUE = 3",
            "classes:",
            "run(self, value: int) -> str",
        ],
    },
    NativeConstructCase {
        filename: "native.ts",
        source: "import { item } from './item';\nexport interface Item { id: string; run(): void; }\nexport const MAX: number = 3;\n",
        required: &[
            "imports:",
            "interface Item",
            "id: string",
            "MAX: number = 3",
        ],
    },
    NativeConstructCase {
        filename: "native.js",
        source: "export class Service { run(value) { return value; } }\nexport const MAX = 3;\n",
        required: &[
            "classes:",
            "export Service",
            "run(value)",
            "consts:",
            "export MAX = 3",
        ],
    },
    NativeConstructCase {
        filename: "native.gleam",
        source: "import gleam/list\npub const max: Int = 3\npub type Color { Red Green }\npub fn run(value: Int) -> Int { value }\n",
        required: &[
            "gleam/list",
            "const max: Int",
            "type Color",
            "Red, Green",
            "fn run(value: Int) -> Int",
        ],
    },
    NativeConstructCase {
        filename: "native.go",
        source: "package demo\nimport \"fmt\"\ntype Service struct { Name string }\nfunc (s Service) Run(value int) string { return fmt.Sprint(value) }\nconst Max int = 3\n",
        required: &[
            "imports:",
            "fmt",
            "struct Service",
            "Name string",
            "impls:",
            "Run(value int) string",
            "Max int",
        ],
    },
    NativeConstructCase {
        filename: "native.html",
        source: "<main id=\"app\" class=\"wide dark\"><article><section id=\"body\"></section></article><script></script></main>\n",
        required: &[
            "<main#app.wide.dark>",
            "<article>",
            "<section#body>",
            "<script>",
        ],
    },
    NativeConstructCase {
        filename: "Native.java",
        source: "package demo;\nimport java.util.List;\npublic interface Run { String run(int value); }\npublic enum Color { RED, GREEN }\n",
        required: &[
            "mod:",
            "demo",
            "java.util.List",
            "traits:",
            "public interface Run",
            "types:",
            "public enum Color",
            "RED, GREEN",
        ],
    },
    NativeConstructCase {
        filename: "native.c",
        source: "#include <stdio.h>\n#define MAX 3\ntypedef struct Point { int x; int y; } Point;\nint run(int value);\n",
        required: &[
            "stdio.h",
            "MAX 3",
            "typedef struct Point Point",
            "int x",
            "int run(int value)",
        ],
    },
    NativeConstructCase {
        filename: "native.h",
        source: "#include <X11/Xlib.h>\n#define MAX_WINDOWS 64\n#define UNUSED(x) (void)(x)\nunion Value { int i; float f; };\ntypedef struct Point { int x; int y; } Point;\ntypedef void (*callback_t)(int);\nchar *window_title(Window window);\nvoid (*on_event)(int code);\nvoid set_callback(item *i, int (*callback)(item *i));\n",
        required: &[
            "X11/Xlib.h",
            "MAX_WINDOWS 64",
            "macros:",
            "UNUSED(x)",
            "union Value",
            "int i",
            "typedef struct Point Point",
            "int x",
            "typedef void (*callback_t)(int)",
            "window_title(Window window)",
            "void (*on_event)(int code)",
            "void set_callback(item *i, int (*callback)(item *i))",
        ],
    },
    NativeConstructCase {
        filename: "native_cpp.h",
        source: "#pragma once\n#include <cstdint>\nusing std::int32_t;\nnamespace ninfer::ops {\nenum class Route { Small, Prompt };\nstruct Invocation { const Tensor* columns = nullptr; int32_t width = 0; };\nconst char* route_name(Route route);\nvoid launch(const Tensor& q, Tensor& out);\n}\n",
        required: &[
            "cstdint",
            "std::int32_t",
            "mod:",
            "ninfer::ops",
            "enum Route",
            "Small, Prompt",
            "struct Invocation",
            "int32_t width = 0",
            "route_name(Route route)",
            "void launch(const Tensor& q, Tensor& out)",
        ],
    },
    NativeConstructCase {
        filename: "native.cpp",
        source: "#include <vector>\nnamespace demo { enum Color { Red, Green }; template<typename T> T run(T value); }\n",
        required: &[
            "vector",
            "mod:",
            "demo",
            "enum Color",
            "Red, Green",
            "template<typename T> T run(T value)",
        ],
    },
    NativeConstructCase {
        filename: "native.cu",
        source: "#include <cuda_runtime.h>\n__constant__ float scale_table[64];\n__constant__ int primes[16] = {2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53};\n__device__ int hit_counter;\n__managed__ int ready = 0;\n__device__ __forceinline__ float half_of(float value) { return value / 2; }\ntemplate <int BLOCK>\n__global__ void __launch_bounds__(256) scale_rows(const float* input, float* output) { output[0] = half_of(input[0]); }\nvoid launch(const float* input, float* output) { scale_rows<256><<<1, 256>>>(input, output); }\n",
        required: &[
            "cuda_runtime.h",
            "consts:",
            "__constant__ float scale_table[64]",
            "__constant__ int primes[16] = {2, 3, 5, 7, 11, 13, 17, 19, [truncated]",
            "__device__ int hit_counter",
            "__managed__ int ready = 0",
            "__device__ __forceinline__ float half_of(float value)",
            "template<int BLOCK> __global__ __launch_bounds__(256) void scale_rows(const float* input, float* output)",
            "void launch(const float* input, float* output)",
        ],
    },
    NativeConstructCase {
        filename: "native.m",
        source: "#import <Foundation/Foundation.h>\n@import CoreGraphics;\n#define MAX_SHAPES 16\n@protocol Drawable <NSObject>\n- (void)drawInContext:(CGContextRef)context;\n@optional\n@property (nonatomic) CGFloat alpha;\n@end\n@interface Canvas : NSObject <Drawable>\n@property (nonatomic, copy) NSString *title;\n- (instancetype)initWithTitle:(NSString *)title\n                     capacity:(NSUInteger)capacity;\n@end\n@interface Canvas (Export)\n- (NSData *)exportPNG;\n@end\nstatic CGFloat clamp_unit(CGFloat value) { return value; }\n#if TARGET_OS_IOS\n@implementation Canvas\n- (NSData *)exportPNG { return [self render]; }\n@end\n#endif\n",
        required: &[
            "Foundation/Foundation.h",
            "CoreGraphics",
            "MAX_SHAPES 16",
            "traits:",
            "@protocol Drawable <NSObject>",
            "- (void)drawInContext:(CGContextRef)context",
            "@property (nonatomic) CGFloat alpha",
            "classes:",
            "@interface Canvas : NSObject <Drawable>",
            "@property (nonatomic, copy) NSString *title",
            "- (instancetype)initWithTitle:(NSString *)title capacity:(NSUInteger)capacity",
            "@interface Canvas (Export)",
            "- (NSData *)exportPNG",
            "CGFloat clamp_unit(CGFloat value)",
            "impls:",
            "@implementation Canvas",
        ],
    },
    NativeConstructCase {
        filename: "native_foundation.m",
        source: "#import \"Shape.h\"\n\nNS_ASSUME_NONNULL_BEGIN\n\ntypedef NS_ENUM(NSInteger, ShapeKind) {\n    ShapeKindCircle,\n    ShapeKindSquare,\n};\n\nNSString *const ShapeErrorDomain = @\"ShapeErrorDomain\";\n\n@interface Shape ()\n@property (nonatomic) ShapeKind kind;\n@end\n\n@implementation Shape\n- (void)draw {}\n@end\n\nNS_ASSUME_NONNULL_END\n",
        required: &[
            "Shape.h",
            "@interface Shape ()",
            "@property (nonatomic) ShapeKind kind",
            "@implementation Shape",
            "- (void)draw",
        ],
    },
    NativeConstructCase {
        filename: "cmake/native.cmake",
        source: "cmake_minimum_required(VERSION 3.20)\nproject(\"Canvas\" VERSION 1.2 LANGUAGES C CXX)\ninclude(cmake/Warnings.cmake)\nfind_package(Threads REQUIRED)\noption(CANVAS_TESTS \"Build the tests\" ON)\nset(CANVAS_SOURCES\n    src/canvas.cpp\n    src/shape.cpp\n)\nmacro(canvas_warn target)\nendmacro()\nif(CANVAS_TESTS)\n  ADD_EXECUTABLE(canvas_cli main.c)\nelse()\n  add_library(canvas STATIC ${CANVAS_SOURCES})\nendif()\n",
        required: &[
            "mod: [2]\n  Canvas",
            "cmake/Warnings.cmake",
            "Threads",
            "option(CANVAS_TESTS \"Build the tests\" ON) [5]",
            "set(CANVAS_SOURCES src/canvas.cpp src/shape.cpp) [6-9]",
            "macro(canvas_warn target) [10-11]",
            "ADD_EXECUTABLE(canvas_cli main.c) [13]",
            "add_library(canvas STATIC ${CANVAS_SOURCES}) [15]",
        ],
    },
    NativeConstructCase {
        filename: "native.proto",
        source: "syntax = \"proto3\";\npackage canvas.v1;\nimport \"google/protobuf/timestamp.proto\";\nimport public \"canvas/v1/shape.proto\";\noption go_package = \"example.com/canvas/v1;canvasv1\";\nmessage Canvas {\n  string id = 1;\n  map<string, string> labels = 2;\n  oneof background {\n    string color = 3;\n    bytes image = 4;\n  }\n  message Layer {\n    string name = 1;\n  }\n}\nextend google.protobuf.FieldOptions {\n  string label = 50000;\n}\nservice CanvasService {\n  rpc GetCanvas(GetCanvasRequest) returns (Canvas);\n  rpc WatchCanvas(stream GetCanvasRequest)\n      returns (stream Canvas) {\n    option deprecated = true;\n  }\n}\n",
        required: &[
            "imports: [3-4]\n  canvas/v1/shape.proto\n  google/protobuf/timestamp.proto",
            "mod: [2]\n  canvas.v1",
            "option go_package = \"example.com/canvas/v1;canvasv1\" [5]",
            "message Canvas [6-16]",
            "map<string, string> labels = 2",
            "oneof background { string color = 3; bytes image = 4 }",
            "message Canvas.Layer [13-15]",
            "extend google.protobuf.FieldOptions [17-19]",
            "string label = 50000",
            "traits:\n  service CanvasService [20-26]",
            "rpc GetCanvas(GetCanvasRequest) returns (Canvas) [21]",
            "rpc WatchCanvas(stream GetCanvasRequest) returns (stream Canvas) [22-25]",
        ],
    },
    NativeConstructCase {
        filename: "many_fields.proto",
        source: "message Many {\n  int32 a = 1;\n  int32 b = 2;\n  int32 c = 3;\n  int32 d = 4;\n  int32 e = 5;\n  int32 f = 6;\n  int32 g = 7;\n  int32 h = 8;\n  int32 i = 9;\n  map<string, int32> j = 10;\n}\n",
        required: &["    int32 h = 8\n    [2 more truncated]"],
    },
    NativeConstructCase {
        filename: "native.csproj",
        source: "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    <TargetFramework>net8.0</TargetFramework>\n  </PropertyGroup>\n  <PropertyGroup Condition=\"'$(Configuration)' == 'Release'\">\n    <Optimize>true</Optimize>\n  </PropertyGroup>\n  <ItemGroup>\n    <PackageReference Include=\"Serilog\" Version=\"3.1.1\" />\n    <PackageReference Include=\"Microsoft.Extensions.DependencyInjection.Abstractions.Testing\" />\n    <Compile Include='src/**/*.cs' />\n    <Content Include=\"wwwroot/**;\n      appsettings.json\" />\n  </ItemGroup>\n  <Target Name=\"Stamp\" BeforeTargets=\"Build\">\n    <Message Text=\"stamping\" />\n  </Target>\n  <section xml:id=\"intro\" />\n</Project>\n",
        required: &[
            "structure:\n  <Project> [1-19]\n    <PropertyGroup> [2-4]\n      <TargetFramework> [3]\n    <PropertyGroup> [5-7]\n      <Optimize> [6]\n    <ItemGroup> [8-14]\n      <PackageReference Include=Serilog> [9]\n      <PackageReference Include=Microsoft.Extensions.DependencyInjection.Abstract[truncated]> [10]\n      <Compile Include=src/**/*.cs> [11]\n      <Content Include=wwwroot/**; appsettings.json> [12-13]\n    <Target Name=Stamp> [15-17]\n      <Message> [16]\n    <section xml:id=intro> [18]",
        ],
    },
    NativeConstructCase {
        filename: "native.plist",
        source: "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>CFBundleName</key>\n\t<string>Canvas</string>\n\t<key>CFBundleVersion</key>\n\t<string>1.2</string>\n\t<key>LSRequiresIPhoneOS</key>\n\t<true/>\n</dict>\n</plist>\n",
        required: &[
            "structure:\n  <plist> [3-12]\n    <dict> [4-11]\n      <key> ×3 [5-9]\n      <string> ×2 [6-8]\n      <true> [10]",
        ],
    },
    NativeConstructCase {
        filename: "native.cs",
        source: "using System;\npublic interface IRun { void Run(); }\npublic enum Color { Red, Green }\npublic record Item(int Id);\n",
        required: &[
            "System",
            "traits:",
            "public interface IRun",
            "void Run()",
            "enum Color",
            "Red, Green",
            "record Item(int Id)",
        ],
    },
    NativeConstructCase {
        filename: "native.rb",
        source: "require 'json'\nmodule Demo\n  class Service\n    def run(value)\n    end\n  end\nend\nVALUE = 3\n",
        required: &[
            "json",
            "mod:",
            "Demo",
            "classes:",
            "Service",
            "run(value)",
            "VALUE = 3",
        ],
    },
    NativeConstructCase {
        filename: "native.php",
        source: "<?php\nuse Demo\\Value;\ninterface Run { public function run(int $value): string; }\nenum Color { case Red; case Green; }\nconst MAX = 3;\n",
        required: &[
            "Demo\\Value",
            "traits:",
            "Run",
            "function run(int $value): string",
            "enum Color",
            "Red, Green",
            "MAX",
        ],
    },
    NativeConstructCase {
        filename: "native.swift",
        source: "import Foundation\npublic protocol Run { func run(value: Int) -> String }\nenum Color { case red, green }\ntypealias Name = String\n",
        required: &[
            "Foundation",
            "traits:",
            "public protocol Run",
            "func run()",
            "enum Color",
            "case red, case green",
            "typealias Name",
        ],
    },
    NativeConstructCase {
        filename: "native.kt",
        source: "package demo\nimport kotlin.collections.List\ninterface Run { fun run(value: Int): String }\nenum class Color { Red, Green }\nconst val MAX: Int = 3\n",
        required: &[
            "demo",
            "kotlin.collections.List",
            "interface Run",
            "fun run(value: Int)",
            "enum class Color",
            "MAX",
        ],
    },
    NativeConstructCase {
        filename: "native.scala",
        source: "package demo\nimport scala.collection.{Map, Set}\ntrait Run { def run(value: Int): String }\nenum Color { case Red, Green }\nval Max: Int = 3\n",
        required: &[
            "demo",
            "scala.collection.{Map, Set}",
            "trait Run",
            "def run(value: Int): String",
            "Color",
            "Red, Green",
            "val Max: Int",
        ],
    },
    NativeConstructCase {
        filename: "native.sh",
        source: "MAX=3\nrun() { echo ok; }\n",
        required: &["MAX = 3", "fns:", "run()"],
    },
    NativeConstructCase {
        filename: "native.lua",
        source: "local json = require('json')\nlocal MAX = 3\nfunction run(value) return value end\n",
        required: &["json", "MAX = 3", "run(value)"],
    },
    NativeConstructCase {
        filename: "native.ex",
        source: "alias Demo.Value\ndefmodule Demo.Service do\n  import Demo.Helpers\n  def run(value), do: value\nend\n",
        required: &[
            "alias: Demo.Value",
            "Demo.Helpers",
            "defmodule Demo.Service",
            "def run(value)",
        ],
    },
    NativeConstructCase {
        filename: "native.md",
        source: "# Main\ntext\n## Detail\ntext\n# Next\n",
        required: &["# Main [1-4]", "## Detail [3-4]", "# Next [5-6]"],
    },
    NativeConstructCase {
        filename: "build/BUILD",
        source: "load(\"//tools:defs.bzl\", \"rule\")\nGROUP = [\"//...\"]\npackage_group(name = \"all\", packages = GROUP)\nexports_files([\"one.txt\"])\nrule(name = \"target\", deprecation = \"old\")\n",
        required: &[
            "loads:",
            "\"//tools:defs.bzl\": rule",
            "package_groups:",
            "all:",
            "exports_files:",
            "variable bindings:",
            "GROUP:",
            "target: rule, deprecated=True",
        ],
    },
    NativeConstructCase {
        filename: "module/MODULE.bazel",
        source: "module(name = \"demo\")\nbazel_dep(name = \"rules\", version = \"1.0\")\next = use_extension(\"//:ext.bzl\", \"ext\")\next.tag(name = \"tag\")\nuse_repo(ext, \"repo\")\nregister_toolchains(\"@repo//:all\", dev_dependency = True)\n",
        required: &[
            "module:",
            "\"@demo\"",
            "bazel_deps:",
            "\"@rules\": \"1.0\"",
            "module_extensions:",
            "ext: \"//:ext.bzl\", \"ext\"",
            "module_extensions.tags:",
            "ext.tag: \"tag\"",
            "repos:",
            "ext: \"@repo\"",
            "register_toolchains:",
        ],
    },
    NativeConstructCase {
        filename: "native.bzl",
        source: "\"\"\"module docs\"\"\"\nload(\"//tools:defs.bzl\", \"rule\")\nVALUE = {\"a\": 1}\ndef run(value = 1):\n    return value\n",
        required: &[
            "module doc:",
            "loads:",
            "\"//tools:defs.bzl\": rule",
            "variable bindings:",
            "VALUE = {\"a\": 1}",
            "functions:",
            "run(value=1)",
        ],
    },
    NativeConstructCase {
        filename: "native.zig",
        source: "//! docs\nconst std = @import(\"std\");\nconst Point = struct { x: i32, y: i32 };\npub fn run(value: i32) i32 { return value; }\ntest \"works\" {}\n",
        required: &[
            "module doc:",
            "std",
            "struct Point",
            "x: i32",
            "run(value: i32) i32",
            "tests:",
        ],
    },
    NativeConstructCase {
        filename: "native.nix",
        source: "{ pkgs, ... }: { imports = [ ./one.nix (import ./two.nix) ]; service = { enable = true; run = value: value; }; }\n",
        required: &[
            "imports:",
            "one.nix",
            "two.nix",
            "fns:",
            "pkgs",
            "service",
            "run(value)",
            "enable",
        ],
    },
    NativeConstructCase {
        filename: "native.dart",
        source: "class Service { String run(int value) => '$value'; int field = 3; }\nString top(int value) => '$value';\n",
        required: &[
            "classes:",
            "class Service",
            "run(int value) String",
            "field int",
            "fns:",
            "top(int value) String",
        ],
    },
    NativeConstructCase {
        filename: "native.toml",
        source: "title = \"demo\"\n[package]\nname = \"demo\"\nversion = \"1\"\n[[bin]]\nname = \"demo\"\n",
        required: &[
            "title = \"demo\"",
            "[package]",
            "name = \"demo\"",
            "version = \"1\"",
            "[[bin]]",
        ],
    },
    NativeConstructCase {
        filename: "native.yaml",
        source: "services:\n  - name: api\n    port: 80\nsettings:\n  enabled: true\n",
        required: &["services", "name", "port", "settings", "enabled"],
    },
    NativeConstructCase {
        filename: "native.sql",
        source: "CREATE SCHEMA app;\nCREATE TABLE app.users (id INT, name TEXT);\nCREATE VIEW app.names AS SELECT name FROM app.users;\nCREATE INDEX users_name ON app.users (name);\n",
        required: &[
            "mod:",
            "app",
            "TABLE app.users",
            "id INT",
            "name TEXT",
            "VIEW app.names",
            "INDEX users_name ON app.users(name)",
        ],
    },
    NativeConstructCase {
        filename: "native.css",
        source: "@import \"base.css\";\n@media screen { .card { color: red; } }\n.button, .link { display: block; }\n",
        required: &[
            "imports:",
            "\"base.css\"",
            "@media screen",
            ".card",
            ".button, .link",
        ],
    },
    NativeConstructCase {
        filename: "native.json",
        source: "{\"service\": {\"name\": \"api\", \"port\": 80}, \"items\": [{\"id\": 1}]}\n",
        required: &["\"service\"", "\"name\"", "\"port\"", "\"items\"", "\"id\""],
    },
    NativeConstructCase {
        filename: "native.hcl",
        source: "enabled = true\nresource \"service\" \"main\" { name = \"api\" lifecycle { prevent_destroy = true } }\n",
        required: &[
            "enabled = true",
            "resource \"service\" \"main\"",
            "name",
            "lifecycle",
        ],
    },
    NativeConstructCase {
        filename: "native.dockerfile",
        source: "FROM alpine AS build\nRUN echo ok\nCOPY --from=build /bin/app /app\nENTRYPOINT [\"/app\"]\n",
        required: &[
            "FROM alpine AS build",
            "RUN echo ok",
            "COPY --from=build /bin/app /app",
            "ENTRYPOINT [\"/app\"]",
        ],
    },
    NativeConstructCase {
        filename: "native.mk",
        source: "include common.mk\nNAME := demo\nifeq ($(MODE),debug)\nDEBUG = 1\nendif\nbuild: dep\n\tcargo build\n",
        required: &[
            "common.mk",
            "NAME := demo",
            "DEBUG = 1",
            "targets:",
            "build:",
        ],
    },
];

const RETURN_TYPE_CASES: &[NativeConstructCase] = &[
    NativeConstructCase {
        filename: "return_types.c",
        source: "const char *label(int code);\nchar **split(const char *text);\nstatic inline const char* inline_label(int code) { return 0; }\nchar const* suffixed(void);\nunsigned long *const fixed(void);\nint (*handler(void))(int);\n",
        required: &[
            "const char *label(int code)",
            "char **split(const char *text)",
            "const char* inline_label(int code)",
            "char const* suffixed(void)",
            "unsigned long *const fixed(void)",
            "int (*handler(void))(int)",
        ],
    },
    NativeConstructCase {
        filename: "return_types.hpp",
        source: "const std::string& name(int id);\nstd::string&& take();\nauto trailing() -> const char*;\ntemplate <typename T> const T& pick(const T& a) { return a; }\nclass Widget {\n  const char* label() const { return nullptr; }\n  Widget& self() & { return *this; }\n};\nconst char* Widget::describe() const { return nullptr; }\n",
        required: &[
            "const std::string& name(int id)",
            "std::string&& take()",
            "auto trailing() -> const char*",
            "template<typename T> const T& pick(const T& a)",
            "const char* label() const",
            "Widget& self() &",
            "const char* Widget::describe() const",
        ],
    },
    NativeConstructCase {
        filename: "return_types.cu",
        source: "__host__ __device__ const float& at(float* data, int i) { return data[i]; }\ntemplate <int BLOCK>\n__global__ void __launch_bounds__(256) scale(float* data) { }\n",
        required: &[
            "__host__ __device__ const float& at(float* data, int i)",
            "template<int BLOCK> __global__ __launch_bounds__(256) void scale(float* data)",
        ],
    },
    NativeConstructCase {
        filename: "return_types.m",
        source: "static const char *shape_label(int kind) { return 0; }\n",
        required: &["const char *shape_label(int kind)"],
    },
];

const PARITY_CASES: &[ParityCase] = &[
    ParityCase {
        filename: "parity.php",
        source: "<?php\nenum Status: string implements JsonSerializable {\n    case Active;\n}\n",
        expected: "types:\n  enum Status: string implements JsonSerializable [2-4]\n    Active",
    },
    ParityCase {
        filename: "parity.py",
        source: "from ..pkg import Item\nHTTP2_PORT = 443\nMAX_PORT = 80\n",
        expected: "imports: [1]\n  pkg.Item\n\nconsts:\n  MAX_PORT = 80 [3]",
    },
    ParityCase {
        filename: "parity.bzl",
        source: "'contains \"\"\" but is not a docstring'\nFOO = 1\n",
        expected: "variable bindings:\n  FOO = 1 [2]",
    },
    ParityCase {
        filename: "module-parity/MODULE.bazel",
        source: "EXT = use_extension(*[\"//:ext.bzl\", \"ext\"])\n",
        expected: "",
    },
    ParityCase {
        filename: "parity.html",
        source: "<div class=\"a\u{a0}b\"></div>\n",
        expected: "structure:\n  <div.a\u{a0}b> [1]",
    },
    ParityCase {
        filename: "Parity.java",
        source: "package\tcom.example;\nimport\tjava.util.List;\nclass Demo {}\n",
        expected: "imports: [2]\n  java.util.List\n\nmod: [1]\n  com.example\n\nclasses:\n  class Demo [3]",
    },
    ParityCase {
        filename: "parity.cs",
        source: "using\tSystem;\nclass Demo :\u{a0}Base {}\n",
        expected: "imports: [1]\n  System\n\nclasses:\n  class Demo : \u{a0}Base [2]",
    },
    ParityCase {
        filename: "parity.rb",
        source: "require \"\"\n",
        expected: "",
    },
    ParityCase {
        filename: "parity.lua",
        source: "require(\"\")\n",
        expected: "imports: [1]\n  \"\"",
    },
    ParityCase {
        filename: "parity.md",
        source: "# \u{a0}Title\u{a0}\n",
        expected: "headings:\n  # \u{a0}Title\u{a0} [1-2]",
    },
    ParityCase {
        filename: "parity.kt",
        source: "class Service {\n    companion object {\n        val DEFAULT: Int = 1\n        fun create(): Service = Service()\n    }\n}\n",
        expected: "classes:\n  class Service [1-6]\n    companion.val DEFAULT [3]\n    companion.fun create() [4]",
    },
    ParityCase {
        filename: "parity.dart",
        source: "enum Result<T> { ok }\n",
        expected: "types:\n  enum Result [1]",
    },
    ParityCase {
        filename: "parity.nix",
        source: "{ demo = pkgs.stdenv.mkDerivation { pname = \"''pkg''\"; }; }\n",
        expected: "consts:\n  demo (pkg) [1]",
    },
    ParityCase {
        filename: "parity.yaml",
        source: "\"'key'\": value\nkey\u{a0}: value\n",
        expected: "consts:\n  key [1]\n  key\u{a0} [2]",
    },
    ParityCase {
        filename: "parity.css",
        source: "@import\"base.css\";\n",
        expected: "imports: [1]\n  @import\"base.css\"",
    },
    ParityCase {
        filename: "parity.mk",
        source: "ifdef X\n\tNAME ::= value\nendif\n",
        expected: "",
    },
    ParityCase {
        filename: "parity.zig",
        source: "const root = @import(\"/\");\n",
        expected: "consts:\n  const root [1]",
    },
    ParityCase {
        filename: "parity.go",
        source: "package demo\nimport \"\"\n",
        expected: "imports: [2]\n  \"\"",
    },
    ParityCase {
        filename: "parity.swift",
        source: "class Plain {}\nclass Child: Parent {}\n",
        expected: "classes:\n  class Plain [1]\n  class Child: Parent [2]",
    },
    ParityCase {
        filename: "parity-space.css",
        source: ".\u{a0} { color: red; }\n",
        expected: "rules:\n  .\u{a0} [1]",
    },
    ParityCase {
        filename: "parity-space.mk",
        source: "NAME = value\u{a0}\n",
        expected: "consts:\n  NAME = value\u{a0} [1]",
    },
];

fn output_line(
    output_line: usize,
    text: &str,
    semantic: IndexLineSemantic,
    body: Option<&str>,
    source_range: Option<(usize, usize)>,
) -> IndexOutputLine {
    IndexOutputLine {
        output_line,
        text: text.to_owned(),
        semantic,
        body: body.map(str::to_owned),
        source_range: source_range.map(|(start_line, end_line)| IndexSourceRange {
            start_line,
            end_line,
        }),
    }
}

type MetadataSpec<'a> = (IndexLineSemantic, Option<&'a str>, Option<(usize, usize)>);

const SECTION_METADATA: MetadataSpec<'static> = (IndexLineSemantic::Section, None, None);
const PLAIN_METADATA: MetadataSpec<'static> = (IndexLineSemantic::Plain, None, None);

const fn item_metadata(body: &str, start: usize, end: usize) -> MetadataSpec<'_> {
    (IndexLineSemantic::Item, Some(body), Some((start, end)))
}

const fn section_metadata(body: &str, start: usize, end: usize) -> MetadataSpec<'_> {
    (IndexLineSemantic::Section, Some(body), Some((start, end)))
}

fn assert_exact_file_output(
    filename: &str,
    output: IndexOutput,
    expected_skeleton: &str,
    metadata: &[MetadataSpec<'_>],
) {
    let IndexOutput::File {
        skeleton,
        lines,
        parse_error,
        truncated,
        ..
    } = output
    else {
        panic!("{filename}: expected file")
    };
    assert_eq!(skeleton, expected_skeleton, "{filename}");
    let expected_lines = expected_skeleton.split('\n').collect::<Vec<_>>();
    assert_eq!(lines.len(), expected_lines.len(), "{filename}");
    assert_eq!(metadata.len(), expected_lines.len(), "{filename}");
    for (index, ((line, text), (semantic, body, range))) in
        lines.iter().zip(expected_lines).zip(metadata).enumerate()
    {
        assert_eq!(
            line,
            &output_line(index + 1, text, *semantic, *body, *range),
            "{filename} line {}",
            index + 1
        );
    }
    assert!(!parse_error, "{filename}");
    assert!(!truncated, "{filename}");
}

#[tokio::test]
async fn rust_index_preserves_compact_skeleton_and_semantic_ranges() {
    let root = tempdir().expect("root");
    fs::write(
        root.path().join("lib.rs"),
        "use std::io;\n\npub fn run(x: i32) -> i32 { x }\n",
    )
    .expect("source");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    let output = group
        .index(
            IndexInput {
                path: "lib.rs".into(),
            },
            &CancellationToken::new(),
        )
        .await
        .expect("index");
    let IndexOutput::File {
        language,
        skeleton,
        lines,
        source_line_count,
        parse_error,
        truncated,
        ..
    } = output
    else {
        panic!("expected file output")
    };
    assert_eq!(language, "rust");
    assert_eq!(
        skeleton,
        "imports: [1]\n  std::io\n\nfns:\n  pub run(x: i32) -> i32 [3]"
    );
    assert_eq!(source_line_count, 4);
    assert!(!parse_error);
    assert!(!truncated);
    assert_eq!(lines[0].semantic, IndexLineSemantic::Section);
    assert_eq!(lines[4].source_range.unwrap().start_line, 3);
}

#[tokio::test]
async fn html_and_all_bazel_modes_preserve_exact_inferred_metadata() {
    let root = tempdir().expect("root");
    for (path, source) in [
        (
            "page.html",
            "<main id=\"app\">\n  <section class=\"card\"></section>\n</main>\n",
        ),
        ("BUILD", "cc_library(name = \"core\")\n"),
        ("MODULE.bazel", "module(name = \"demo\")\n"),
        ("defs.bzl", "def run(value):\n    return value\n"),
    ] {
        fs::write(root.path().join(path), source).expect("source");
    }
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    let cases = [
        (
            "page.html",
            vec![
                output_line(1, "structure:", IndexLineSemantic::Section, None, None),
                output_line(
                    2,
                    "  <main#app> [1-3]",
                    IndexLineSemantic::Item,
                    Some("  <main#app>"),
                    Some((1, 3)),
                ),
                output_line(
                    3,
                    "    <section.card> [2]",
                    IndexLineSemantic::Item,
                    Some("    <section.card>"),
                    Some((2, 2)),
                ),
            ],
        ),
        (
            "BUILD",
            vec![
                output_line(1, "targets:", IndexLineSemantic::Section, None, None),
                output_line(
                    2,
                    "    core: cc_library [1]",
                    IndexLineSemantic::Item,
                    Some("    core: cc_library"),
                    Some((1, 1)),
                ),
            ],
        ),
        (
            "MODULE.bazel",
            vec![
                output_line(1, "module:", IndexLineSemantic::Section, None, None),
                output_line(
                    2,
                    "  \"@demo\" [1]",
                    IndexLineSemantic::Item,
                    Some("  \"@demo\""),
                    Some((1, 1)),
                ),
            ],
        ),
        (
            "defs.bzl",
            vec![
                output_line(1, "functions:", IndexLineSemantic::Section, None, None),
                output_line(
                    2,
                    "  run(value) [1-2]",
                    IndexLineSemantic::Item,
                    Some("  run(value)"),
                    Some((1, 2)),
                ),
            ],
        ),
    ];
    for (path, expected) in cases {
        let output = group
            .index(IndexInput { path: path.into() }, &CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{path}: {error}"));
        let IndexOutput::File { lines, .. } = output else {
            panic!("{path}: expected file")
        };
        assert_eq!(lines, expected, "{path}");
    }
}

#[tokio::test]
async fn native_output_orders_sections_and_omits_function_body_usage() {
    let root = tempdir().expect("root");
    fs::write(
        root.path().join("ordered.rs"),
        "pub const MAX: usize = 1;\npub fn run() { let hidden = MAX; helper(hidden); }\npub struct Item { value: usize }\n",
    )
    .expect("source");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    let output = group
        .index(
            IndexInput {
                path: "ordered.rs".into(),
            },
            &CancellationToken::new(),
        )
        .await
        .expect("index");
    let IndexOutput::File {
        skeleton, lines, ..
    } = output
    else {
        panic!("expected file")
    };
    assert_eq!(
        skeleton,
        "consts:\n  pub MAX: usize [1]\n\ntypes:\n  pub struct Item [3]\n    value: usize\n\nfns:\n  pub run() [2]"
    );
    assert!(!skeleton.contains("hidden"));
    assert!(!skeleton.contains("helper"));
    assert_eq!(
        lines[1].source_range,
        Some(IndexSourceRange {
            start_line: 1,
            end_line: 1
        })
    );
    assert_eq!(
        lines[8].source_range,
        Some(IndexSourceRange {
            start_line: 2,
            end_line: 2
        })
    );
}

#[tokio::test]
async fn import_merging_section_order_and_field_truncation_match_maki_bytes() {
    let root = tempdir().expect("root");
    fs::write(
        root.path().join("many.rs"),
        "use std::io;\nuse std::fs;\npub struct Many {\n    a: i32,\n    b: i32,\n    c: i32,\n    d: i32,\n    e: i32,\n    f: i32,\n    g: i32,\n    h: i32,\n    i: i32,\n    j: i32,\n}\n",
    )
    .expect("source");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    let output = group
        .index(
            IndexInput {
                path: "many.rs".into(),
            },
            &CancellationToken::new(),
        )
        .await
        .expect("index");
    let IndexOutput::File {
        skeleton,
        lines,
        truncated,
        ..
    } = output
    else {
        panic!("expected file")
    };
    assert_eq!(
        skeleton,
        "imports: [1-2]\n  std::{fs, io}\n\ntypes:\n  pub struct Many [3-14]\n    a: i32\n    b: i32\n    c: i32\n    d: i32\n    e: i32\n    f: i32\n    g: i32\n    h: i32\n    [2 more truncated]"
    );
    assert!(truncated);
    assert_eq!(lines.last().unwrap().semantic, IndexLineSemantic::Dimmed);
}

#[tokio::test]
async fn representative_maki_language_corpus_is_supported() {
    let root = tempdir().expect("root");
    for case in LANGUAGE_CASES {
        fs::write(root.path().join(case.filename), case.source).expect("source");
    }
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    for case in LANGUAGE_CASES {
        let output = group
            .index(
                IndexInput {
                    path: case.filename.into(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", case.filename));
        let IndexOutput::File {
            language, skeleton, ..
        } = output
        else {
            panic!("{}: expected file", case.filename)
        };
        assert_eq!(language, case.language, "{}", case.filename);
        assert_eq!(skeleton, case.expected, "{}", case.filename);
    }
}

#[tokio::test]
async fn native_extractors_cover_representative_language_constructs() {
    assert_required_constructs(NATIVE_CONSTRUCT_CASES).await;
}

#[tokio::test]
async fn c_family_signatures_keep_the_declared_return_type_and_qualifiers() {
    assert_required_constructs(RETURN_TYPE_CASES).await;
}

async fn assert_required_constructs(cases: &[NativeConstructCase]) {
    let root = tempdir().expect("root");
    for case in cases {
        if let Some(parent) = root.path().join(case.filename).parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(root.path().join(case.filename), case.source).expect("source");
    }
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    for case in cases {
        let output = group
            .index(
                IndexInput {
                    path: case.filename.into(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", case.filename));
        let IndexOutput::File { skeleton, .. } = output else {
            panic!("{}: expected file", case.filename)
        };
        for required in case.required {
            assert!(
                skeleton.contains(required),
                "{}: missing {required:?} in:\n{skeleton}",
                case.filename
            );
        }
    }
}

#[tokio::test]
async fn native_extractors_match_retained_lua_parity_goldens() {
    let root = tempdir().expect("root");
    for case in PARITY_CASES {
        if let Some(parent) = root.path().join(case.filename).parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(root.path().join(case.filename), case.source).expect("source");
    }
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    for case in PARITY_CASES {
        let output = group
            .index(
                IndexInput {
                    path: case.filename.into(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{}: {error}", case.filename));
        let IndexOutput::File { skeleton, .. } = output else {
            panic!("{}: expected file", case.filename)
        };
        assert_eq!(skeleton, case.expected, "{}", case.filename);
    }
}

#[tokio::test]
async fn authorized_index_rejects_a_path_type_swap() {
    let root = tempdir().expect("root");
    let path = root.path().join("target");
    fs::create_dir(&path).expect("directory");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    let input = IndexInput {
        path: "target".into(),
    };
    let resource = group.inspect_index(&input).await.expect("inspection");
    assert_eq!(resource.access, FileResourceAccess::Traverse);
    fs::remove_dir(&path).expect("remove directory");
    fs::write(&path, "fn changed() {}\n").expect("replacement file");

    let error = group
        .index_authorized_with_configuration(
            resource,
            IndexExecutionConfiguration::default(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("type swap must fail");

    assert!(error.to_string().contains(INDEX_TYPE_CHANGED));
}

#[tokio::test]
async fn prepared_index_keeps_the_exact_path_and_configuration() {
    let root = tempdir().expect("root");
    fs::write(root.path().join("source.rs"), "fn source() {}\n").expect("source");
    fs::write(root.path().join("other.rs"), "fn other() {}\n").expect("other");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    let mut input = IndexInput {
        path: "source.rs".into(),
    };
    let mut configuration = IndexExecutionConfiguration::default();
    configuration.limits.max_model_output_bytes = 1_024;
    configuration.limits.max_output_line_bytes = 512;
    let prepared = group
        .prepare_index_with_configuration(input.clone(), configuration, &CancellationToken::new())
        .await
        .expect("prepared index");
    assert_eq!(prepared.relative_path(), "source.rs");
    assert!(prepared.retained_bytes() >= prepared.resource().requested_path.len());
    input.path = "other.rs".into();
    configuration.limits.max_model_output_bytes = 1;
    assert_eq!(configuration.limits.max_model_output_bytes, 1);

    let output = group
        .execute_prepared_index(prepared, &CancellationToken::new())
        .await
        .expect("index");
    let IndexOutput::File {
        relative_path,
        skeleton,
        ..
    } = output
    else {
        panic!("expected file");
    };
    assert_eq!(relative_path, "source.rs");
    assert!(skeleton.contains("source"));
    assert!(!skeleton.contains("other"));
}

#[tokio::test]
async fn complex_native_cases_have_exact_output_ranges_and_metadata() {
    let cases: &[(&str, &str, &[MetadataSpec<'_>])] = &[
        (
            "module/MODULE.bazel",
            "module:\n  \"@demo\" [1]\n\nbazel_deps:\n  \"@rules\": \"1.0\" [2]\n\nmodule_extensions:\n  ext: \"//:ext.bzl\", \"ext\" [3]\n\nmodule_extensions.tags:\n  ext.tag: \"tag\" [4]\n\nrepos:\n  ext: \"@repo\" [5]\n\nregister_toolchains:\n  \"@repo//:all\": dev=True [6]",
            &[
                SECTION_METADATA,
                item_metadata("  \"@demo\"", 1, 1),
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  \"@rules\": \"1.0\"", 2, 2),
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  ext: \"//:ext.bzl\", \"ext\"", 3, 3),
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  ext.tag: \"tag\"", 4, 4),
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  ext: \"@repo\"", 5, 5),
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  \"@repo//:all\": dev=True", 6, 6),
            ],
        ),
        (
            "native.yaml",
            "consts:\n  services [1-3]\n    name [2]\n    port [3]\n  settings [4-6]\n    enabled [5]",
            &[
                SECTION_METADATA,
                item_metadata("  services", 1, 3),
                item_metadata("    name", 2, 2),
                item_metadata("    port", 3, 3),
                item_metadata("  settings", 4, 6),
                item_metadata("    enabled", 5, 5),
            ],
        ),
        (
            "native.sql",
            "mod: [1]\n  app\n\ntypes:\n  VIEW app.names [3]\n    SELECT name FROM app.users\n\nfns:\n  INDEX users_name ON app.users(name) [4]\n\nclasses:\n  TABLE app.users [2]\n    id INT\n    name TEXT",
            &[
                section_metadata("mod: ", 1, 1),
                PLAIN_METADATA,
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  VIEW app.names", 3, 3),
                PLAIN_METADATA,
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  INDEX users_name ON app.users(name)", 4, 4),
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  TABLE app.users", 2, 2),
                PLAIN_METADATA,
                PLAIN_METADATA,
            ],
        ),
        (
            "native.nix",
            "imports: [1]\n  ./{one.nix, two.nix}\n\nconsts:\n  service [1]\n    enable [1]\n    run(value) [1]\n\nfns:\n  fns(pkgs, ...) [1]",
            &[
                section_metadata("imports: ", 1, 1),
                PLAIN_METADATA,
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  service", 1, 1),
                item_metadata("    enable", 1, 1),
                item_metadata("    run(value)", 1, 1),
                PLAIN_METADATA,
                SECTION_METADATA,
                item_metadata("  fns(pkgs, ...)", 1, 1),
            ],
        ),
    ];
    let root = tempdir().expect("root");
    for (filename, _, _) in cases {
        let case = NATIVE_CONSTRUCT_CASES
            .iter()
            .find(|case| case.filename == *filename)
            .expect("native case");
        if let Some(parent) = root.path().join(filename).parent() {
            fs::create_dir_all(parent).expect("fixture parent");
        }
        fs::write(root.path().join(filename), case.source).expect("source");
    }
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    for (filename, expected_skeleton, metadata) in cases {
        let output = group
            .index(
                IndexInput {
                    path: (*filename).into(),
                },
                &CancellationToken::new(),
            )
            .await
            .expect("index");
        assert_exact_file_output(filename, output, expected_skeleton, metadata);
    }
}

#[tokio::test]
async fn every_extension_and_exact_filename_mapping_is_preserved() {
    const EXTENSIONS: &[(&str, &str)] = &[
        ("rs", "rust"),
        ("py", "python"),
        ("pyi", "python"),
        ("ts", "typescript"),
        ("tsx", "typescript"),
        ("mts", "typescript"),
        ("cts", "typescript"),
        ("js", "javascript"),
        ("jsx", "javascript"),
        ("mjs", "javascript"),
        ("cjs", "javascript"),
        ("gleam", "gleam"),
        ("go", "go"),
        ("htm", "html"),
        ("html", "html"),
        ("java", "java"),
        ("c", "c"),
        ("h", "cpp"),
        ("cpp", "cpp"),
        ("cc", "cpp"),
        ("cxx", "cpp"),
        ("c++", "cpp"),
        ("hpp", "cpp"),
        ("hxx", "cpp"),
        ("hh", "cpp"),
        ("h++", "cpp"),
        ("ixx", "cpp"),
        ("cppm", "cpp"),
        ("ccm", "cpp"),
        ("cxxm", "cpp"),
        ("inl", "cpp"),
        ("ipp", "cpp"),
        ("tpp", "cpp"),
        ("tcc", "cpp"),
        ("metal", "cpp"),
        ("cu", "cuda"),
        ("cuh", "cuda"),
        ("m", "objc"),
        ("mm", "objc"),
        ("cs", "c_sharp"),
        ("rb", "ruby"),
        ("rake", "ruby"),
        ("gemspec", "ruby"),
        ("php", "php"),
        ("swift", "swift"),
        ("kt", "kotlin"),
        ("kts", "kotlin"),
        ("scala", "scala"),
        ("sc", "scala"),
        ("sh", "bash"),
        ("bash", "bash"),
        ("zsh", "bash"),
        ("lua", "lua_lang"),
        ("ex", "elixir"),
        ("exs", "elixir"),
        ("md", "markdown"),
        ("markdown", "markdown"),
        ("mdx", "markdown"),
        ("bzl", "bazel_bzl"),
        ("zig", "zig"),
        ("nix", "nix"),
        ("dart", "dart"),
        ("toml", "toml"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
        ("sql", "sql"),
        ("css", "css"),
        ("json", "json"),
        ("jsonc", "json"),
        ("hcl", "hcl"),
        ("tf", "hcl"),
        ("tfvars", "hcl"),
        ("dockerfile", "containerfile"),
        ("mk", "make"),
        ("mak", "make"),
        ("cmake", "cmake"),
        ("proto", "proto"),
        ("xml", "xml"),
        ("xsd", "xml"),
        ("xsl", "xml"),
        ("xslt", "xml"),
        ("xaml", "xml"),
        ("plist", "xml"),
        ("resx", "xml"),
        ("wsdl", "xml"),
        ("csproj", "xml"),
        ("fsproj", "xml"),
        ("vbproj", "xml"),
        ("vcxproj", "xml"),
        ("props", "xml"),
        ("targets", "xml"),
    ];
    const FILENAMES: &[(&str, &str)] = &[
        ("MODULE.bazel", "bazel_module"),
        ("BUILD", "bazel_build"),
        ("BUILD.bazel", "bazel_build"),
        ("Containerfile", "containerfile"),
        ("Dockerfile", "containerfile"),
        ("Containerfile.ci", "containerfile"),
        ("Dockerfile.dev", "containerfile"),
        ("GNUmakefile", "make"),
        ("Makefile", "make"),
        ("makefile", "make"),
        ("CMakeLists.txt", "cmake"),
    ];
    let root = tempdir().expect("root");
    for (extension, _) in EXTENSIONS {
        fs::write(root.path().join(format!("empty.{extension}")), "").expect("source");
    }
    for (filename, _) in FILENAMES {
        fs::write(root.path().join(filename), "").expect("source");
    }
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    for (filename, expected) in EXTENSIONS
        .iter()
        .map(|(extension, language)| (format!("empty.{extension}"), *language))
        .chain(
            FILENAMES
                .iter()
                .map(|(filename, language)| ((*filename).to_owned(), *language)),
        )
    {
        let output = group
            .index(
                IndexInput {
                    path: filename.clone(),
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{filename}: {error}"));
        let IndexOutput::File { language, .. } = output else {
            panic!("{filename}: expected file")
        };
        assert_eq!(language, expected, "{filename}");
    }
}

#[tokio::test]
async fn directory_listing_is_typed_sorted_visible_and_bounded() {
    let root = tempdir().expect("root");
    fs::create_dir(root.path().join("zdir")).expect("directory");
    fs::create_dir(root.path().join("adir")).expect("directory");
    fs::write(root.path().join("z.txt"), "").expect("file");
    fs::write(root.path().join("a.txt"), "").expect("file");
    fs::write(root.path().join("AGENTS.md"), "instructions").expect("file");
    fs::write(root.path().join(".env"), "secret").expect("protected file");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    let output = group
        .index(IndexInput { path: ".".into() }, &CancellationToken::new())
        .await
        .expect("listing");
    let IndexOutput::Directory {
        entries,
        total_count,
        truncated,
        listing,
        ..
    } = output
    else {
        panic!("expected directory")
    };
    assert_eq!(
        listing, "adir/\nzdir/\nAGENTS.md\na.txt\nz.txt",
        "Workcell must not inject or hide harness instruction files"
    );
    assert_eq!(total_count, 5);
    assert!(!truncated);
    assert_eq!(entries[0].kind, IndexDirectoryEntryKind::Directory);
    assert_eq!(entries[2].kind, IndexDirectoryEntryKind::File);

    let limited = group
        .index_with_configuration(
            IndexInput { path: ".".into() },
            IndexExecutionConfiguration {
                limits: IndexLimits {
                    max_directory_entries: 2,
                    ..IndexLimits::default()
                },
            },
            &CancellationToken::new(),
        )
        .await
        .expect("limited listing");
    let IndexOutput::Directory {
        entries,
        total_count,
        truncated,
        listing,
        ..
    } = limited
    else {
        panic!("expected directory")
    };
    assert_eq!(entries.len(), 2);
    assert_eq!(total_count, 5);
    assert!(truncated);
    assert!(listing.ends_with("[truncated]"));

    let scan_root = root.path().join("scan-limited");
    fs::create_dir(&scan_root).expect("scan root");
    for name in ["one", "two", "three", "four"] {
        fs::write(scan_root.join(name), "").expect("scan entry");
    }
    let scan_limited = group
        .index_with_configuration(
            IndexInput {
                path: "scan-limited".into(),
            },
            IndexExecutionConfiguration {
                limits: IndexLimits {
                    max_directory_entries: 2,
                    max_directory_scan_entries: 2,
                    ..IndexLimits::default()
                },
            },
            &CancellationToken::new(),
        )
        .await
        .expect("scan-limited listing");
    let IndexOutput::Directory {
        total_count,
        truncated,
        ..
    } = scan_limited
    else {
        panic!("expected directory")
    };
    assert_eq!(total_count, 2);
    assert!(truncated);
}

#[tokio::test]
async fn host_limits_bound_source_output_lines_nodes_and_depth() {
    let root = tempdir().expect("root");
    fs::write(
        root.path().join("large.rs"),
        "pub fn this_is_a_very_long_函数_name(argument: String) -> String { argument }\n",
    )
    .expect("source");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    let token = CancellationToken::new();

    let source_error = group
        .index_with_configuration(
            IndexInput {
                path: "large.rs".into(),
            },
            IndexExecutionConfiguration {
                limits: IndexLimits {
                    max_source_bytes: 16,
                    ..IndexLimits::default()
                },
            },
            &token,
        )
        .await
        .expect_err("source bound");
    assert!(
        source_error
            .to_string()
            .contains("maximum size of 16 bytes")
    );

    let bounded = group
        .index_with_configuration(
            IndexInput {
                path: "large.rs".into(),
            },
            IndexExecutionConfiguration {
                limits: IndexLimits {
                    max_model_output_bytes: 40,
                    max_output_line_bytes: 24,
                    ..IndexLimits::default()
                },
            },
            &token,
        )
        .await
        .expect("bounded output");
    let IndexOutput::File {
        skeleton,
        lines,
        truncated,
        ..
    } = bounded
    else {
        panic!("expected file")
    };
    assert!(truncated);
    assert!(skeleton.len() <= 40);
    assert!(skeleton.is_char_boundary(skeleton.len()));
    assert!(lines.iter().all(|line| line.text.len() <= 24));

    for (limits, expected) in [
        (
            IndexLimits {
                max_nodes: 1,
                ..IndexLimits::default()
            },
            "node limit",
        ),
        (
            IndexLimits {
                max_depth: 1,
                ..IndexLimits::default()
            },
            "depth limit",
        ),
    ] {
        let error = group
            .index_with_configuration(
                IndexInput {
                    path: "large.rs".into(),
                },
                IndexExecutionConfiguration { limits },
                &token,
            )
            .await
            .expect_err(expected);
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[tokio::test]
async fn rejects_unsupported_binary_and_non_utf8_files_and_honors_cancellation() {
    let root = tempdir().expect("root");
    fs::write(root.path().join("plain.txt"), "text").expect("unsupported");
    fs::write(root.path().join("Dockerfile.dockerignore"), "target\n").expect("ignore file");
    fs::write(root.path().join("binary.rs"), b"source\0binary").expect("binary");
    fs::write(root.path().join("invalid.rs"), b"fn main() {}\n\xff").expect("invalid UTF-8");
    fs::write(root.path().join("valid.rs"), "fn main() {}\n").expect("valid");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    for (path, expected) in [
        ("plain.txt", "Unsupported file type: .txt"),
        (
            "Dockerfile.dockerignore",
            "Unsupported file type: .dockerignore",
        ),
        ("binary.rs", "binary file"),
        ("invalid.rs", "strict UTF-8"),
    ] {
        let error = group
            .index(IndexInput { path: path.into() }, &CancellationToken::new())
            .await
            .expect_err(path);
        assert!(error.to_string().contains(expected), "{path}: {error}");
    }

    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
        group
            .index(
                IndexInput {
                    path: "valid.rs".into()
                },
                &cancellation
            )
            .await,
        Err(FilesystemError::Aborted)
    ));
}

#[tokio::test]
async fn parser_recovery_retains_best_effort_output_and_records_errors() {
    let root = tempdir().expect("root");
    fs::write(
        root.path().join("broken.rs"),
        "pub struct Valid { value: i32 }\npub fn broken( {\n",
    )
    .expect("source");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    let output = group
        .index(
            IndexInput {
                path: "broken.rs".into(),
            },
            &CancellationToken::new(),
        )
        .await
        .expect("best effort output");
    let IndexOutput::File {
        skeleton,
        parse_error,
        ..
    } = output
    else {
        panic!("expected file")
    };
    assert!(parse_error);
    assert!(skeleton.contains("struct Valid"));
}

#[tokio::test]
async fn parser_deadline_and_in_flight_cancellation_abort_large_parses() {
    let root = tempdir().expect("root");
    let source = "pub fn generated(value: i32) -> i32 { value }\n".repeat(30_000);
    fs::write(root.path().join("generated.rs"), source).expect("source");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");
    let deadline = group
        .index_with_configuration(
            IndexInput {
                path: "generated.rs".into(),
            },
            IndexExecutionConfiguration {
                limits: IndexLimits {
                    parser_deadline_ms: 1,
                    ..IndexLimits::default()
                },
            },
            &CancellationToken::new(),
        )
        .await
        .expect_err("deadline");
    assert!(deadline.to_string().contains("deadline"), "{deadline}");

    let cancellation = CancellationToken::new();
    let running = {
        let group = group.clone();
        let cancellation = cancellation.clone();
        tokio::spawn(async move {
            group
                .index(
                    IndexInput {
                        path: "generated.rs".into(),
                    },
                    &cancellation,
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    cancellation.cancel();
    assert!(matches!(
        running.await.expect("join"),
        Err(FilesystemError::Aborted)
    ));
}

#[tokio::test]
async fn inspect_resolves_one_existing_resource_without_reading() {
    let root = tempdir().expect("root");
    fs::write(root.path().join("source.rs"), b"source\0binary").expect("source");
    fs::create_dir(root.path().join("src")).expect("directory");
    let group = FileToolGroup::new(root.path(), false, None)
        .await
        .expect("group");

    let file = group
        .inspect_index(&IndexInput {
            path: "source.rs".into(),
        })
        .await
        .expect("inspect file without content classification");
    assert_eq!(file.access, FileResourceAccess::Read);
    let directory = group
        .inspect_index(&IndexInput { path: "src".into() })
        .await
        .expect("inspect directory");
    assert_eq!(directory.access, FileResourceAccess::Traverse);
    assert!(
        group
            .inspect_index(&IndexInput {
                path: "missing.rs".into()
            })
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn confined_and_unconfined_modes_preserve_path_and_visibility_policy() {
    use std::os::unix::fs::symlink;

    let temporary = tempdir().expect("temporary");
    let root = temporary.path().join("root");
    let outside = temporary.path().join("outside");
    fs::create_dir(&root).expect("root");
    fs::create_dir(&outside).expect("outside");
    fs::write(root.join("inside.rs"), "fn inside() {}\n").expect("inside");
    fs::write(root.join(".env"), "protected").expect("protected");
    fs::write(outside.join("outside.rs"), "fn outside() {}\n").expect("outside");
    fs::write(outside.join(".env"), "visible when unconfined").expect("protected outside");
    symlink(&outside, root.join("escape")).expect("escape link");
    symlink(root.join("inside.rs"), root.join("linked.rs")).expect("inside link");
    let confined = FileToolGroup::new(&root, false, None)
        .await
        .expect("confined");

    for path in ["../outside/outside.rs", "escape/outside.rs"] {
        assert!(matches!(
            confined
                .index(IndexInput { path: path.into() }, &CancellationToken::new())
                .await,
            Err(FilesystemError::RootEscape(_))
        ));
    }
    assert!(matches!(
        confined
            .index(
                IndexInput {
                    path: ".env".into()
                },
                &CancellationToken::new()
            )
            .await,
        Err(FilesystemError::ProtectedPath(_))
    ));
    confined
        .index(
            IndexInput {
                path: root.join("inside.rs").to_string_lossy().into_owned(),
            },
            &CancellationToken::new(),
        )
        .await
        .expect("absolute path inside confined root");
    let linked = confined
        .index(
            IndexInput {
                path: "linked.rs".into(),
            },
            &CancellationToken::new(),
        )
        .await
        .expect("internal symlink");
    let IndexOutput::File { path, .. } = linked else {
        panic!("expected file")
    };
    assert_eq!(PathBuf::from(path), root.join("inside.rs"));

    let unconfined = FileToolGroup::new_unconfined(&root, false, None)
        .await
        .expect("unconfined");
    let output = unconfined
        .index(
            IndexInput {
                path: outside.to_string_lossy().into_owned(),
            },
            &CancellationToken::new(),
        )
        .await
        .expect("absolute unconfined directory");
    let IndexOutput::Directory { listing, .. } = output else {
        panic!("expected directory")
    };
    assert_eq!(listing, ".env\noutside.rs");
}
