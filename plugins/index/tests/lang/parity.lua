local th = require("maki.test_helpers")
local helpers = require("tests.helpers")
local case = th.case
local idx = helpers.idx

local nbsp = string.char(194, 160)

local cases = {
  {
    "php_enum_interfaces",
    "php",
    "<?php\nenum Status: string implements JsonSerializable {\n    case Active;\n}\n",
    "types:\n  enum Status: string implements JsonSerializable [2-4]\n    Active",
  },
  {
    "python_relative_import_and_numbered_constant",
    "python",
    "from ..pkg import Item\nHTTP2_PORT = 443\nMAX_PORT = 80\n",
    "imports: [1]\n  pkg.Item\n\nconsts:\n  MAX_PORT = 80 [3]",
  },
  {
    "bazel_ordinary_string_is_not_doc",
    "bazel_bzl",
    '\'contains """ but is not a docstring\'\nFOO = 1\n',
    "variable bindings:\n  FOO = 1 [2]",
  },
  {
    "bazel_unresolved_extension_is_not_variable",
    "bazel_module",
    'EXT = use_extension(*["//:ext.bzl", "ext"])\n',
    "",
  },
  {
    "html_non_ascii_class_space",
    "html",
    '<div class="a' .. nbsp .. 'b"></div>\n',
    "structure:\n  <div.a" .. nbsp .. "b> [1]",
  },
  {
    "java_keyword_tab",
    "java",
    "package\tcom.example;\nimport\tjava.util.List;\nclass Demo {}\n",
    "imports: [2]\n  java.util.List\n\nmod: [1]\n  com.example\n\nclasses:\n  class Demo [3]",
  },
  {
    "csharp_keyword_tab_and_non_ascii_base_space",
    "c_sharp",
    "using\tSystem;\nclass Demo :" .. nbsp .. "Base {}\n",
    "imports: [1]\n  System\n\nclasses:\n  class Demo : " .. nbsp .. "Base [2]",
  },
  { "ruby_empty_require", "ruby", 'require ""\n', "" },
  { "lua_empty_require", "lua_lang", 'require("")\n', 'imports: [1]\n  ""' },
  {
    "markdown_non_ascii_heading_space",
    "markdown",
    "# " .. nbsp .. "Title" .. nbsp .. "\n",
    "headings:\n  # " .. nbsp .. "Title" .. nbsp .. " [1-2]",
  },
  {
    "kotlin_companion_ranges",
    "kotlin",
    "class Service {\n    companion object {\n        val DEFAULT: Int = 1\n        fun create(): Service = Service()\n    }\n}\n",
    "classes:\n  class Service [1-6]\n    companion.val DEFAULT [3]\n    companion.fun create() [4]",
  },
  { "dart_generic_enum", "dart", "enum Result<T> { ok }\n", "types:\n  enum Result [1]" },
  {
    "nix_nested_string_delimiters",
    "nix",
    "{ demo = pkgs.stdenv.mkDerivation { pname = \"''pkg''\"; }; }\n",
    "consts:\n  demo (pkg) [1]",
  },
  {
    "yaml_nested_quotes_and_non_ascii_space",
    "yaml",
    "\"'key'\": value\nkey" .. nbsp .. ": value\n",
    "consts:\n  key [1]\n  key" .. nbsp .. " [2]",
  },
  {
    "css_import_without_space",
    "css",
    '@import"base.css";\n',
    'imports: [1]\n  @import"base.css"',
  },
  { "make_recovered_double_operator", "make", "ifdef X\n\tNAME ::= value\nendif\n", "" },
  { "zig_empty_import_path", "zig", 'const root = @import("/");\n', "consts:\n  const root [1]" },
  { "go_empty_import", "go", 'package demo\nimport ""\n', 'imports: [2]\n  ""' },
  {
    "swift_inheritance_spacing",
    "swift",
    "class Plain {}\nclass Child: Parent {}\n",
    "classes:\n  class Plain [1]\n  class Child: Parent [2]",
  },
  {
    "css_non_ascii_identifier_space",
    "css",
    "." .. nbsp .. " { color: red; }\n",
    "rules:\n  ." .. nbsp .. " [1]",
  },
  {
    "make_non_ascii_value_space",
    "make",
    "NAME = value" .. nbsp .. "\n",
    "consts:\n  NAME = value" .. nbsp .. " [1]",
  },
}

for _, fixture in ipairs(cases) do
  case("parity_" .. fixture[1], function()
    local output = idx(fixture[3], fixture[2])
    output = output:gsub("\n$", "")
    assert(
      output == fixture[4],
      fixture[1] .. " mismatch:\n--- expected ---\n" .. fixture[4] .. "\n--- got ---\n" .. output
    )
  end)
end
