-- Exercises the pure caudra.fs, caudra.ui, caudra.env, and caudra.uv helpers.

local failures = {}

local function case(name, fn)
  local ok, err = pcall(fn)
  if not ok then
    failures[#failures + 1] = name .. ": " .. tostring(err)
  end
end

local function line_text(spans)
  local parts = {}
  for _, span in ipairs(spans) do
    parts[#parts + 1] = span[1]
  end
  return table.concat(parts)
end

local function lines_text(lines)
  local out = {}
  for _, spans in ipairs(lines) do
    out[#out + 1] = line_text(spans)
  end
  return table.concat(out, "\n")
end

case("joinpath_joins_every_segment", function()
  local joined = caudra.fs.joinpath("src", "api", "fs.rs")
  assert(caudra.fs.basename(joined) == "fs.rs", joined)
  assert(caudra.fs.basename(caudra.fs.dirname(joined)) == "api", joined)
end)

case("normalize_resolves_dot_segments_into_an_absolute_path", function()
  local base = caudra.uv.cwd()
  local normalized = caudra.fs.normalize("a/./b/../c")
  assert(normalized == caudra.fs.joinpath(base, "a", "c"), normalized)
end)

case("parents_lists_every_ancestor_nearest_first", function()
  local dir = caudra.fs.joinpath(caudra.uv.cwd(), "a", "b")
  local parents = caudra.fs.parents(caudra.fs.joinpath(dir, "c"))
  assert(parents[1] == dir, tostring(parents[1]))
  assert(parents[2] == caudra.fs.dirname(dir), tostring(parents[2]))
  assert(#caudra.fs.parents(parents[#parents]) == 0, "last parent is not the root")
end)

case("root_finds_the_nearest_directory_holding_a_marker", function()
  local here = caudra.fs.joinpath(caudra.uv.cwd(), "Cargo.toml")
  assert(caudra.fs.root(here, "Cargo.toml") == caudra.uv.cwd(), "single marker")
  assert(caudra.fs.root(here, { "no-such-marker", "Cargo.toml" }) == caudra.uv.cwd(), "marker list")
  assert(caudra.fs.root(here, "no-such-marker-anywhere") == nil, "missing marker")
end)

case("path_accessors_answer_absolute_paths", function()
  for name, path in pairs({ home = caudra.uv.os_homedir(), config = caudra.env.config_dir() }) do
    assert(type(path) == "string", name)
    assert(caudra.fs.abspath(path) == path, name .. ": " .. path)
  end
end)

case("display_width_counts_wide_glyphs_twice", function()
  assert(caudra.ui.display_width("abc") == 3)
  assert(caudra.ui.display_width("日本") == 4)
end)

case("truncate_text_splits_at_a_cell_boundary", function()
  local ascii = caudra.ui.truncate_text("hello world", 5)
  assert(ascii.head == "hello" and ascii.tail == " world", ascii.head .. "|" .. ascii.tail)
  local wide = caudra.ui.truncate_text("日本語", 3)
  assert(wide.head == "日" and wide.tail == "本語", wide.head .. "|" .. wide.tail)
end)

case("humantime_formats_compactly", function()
  assert(caudra.ui.humantime(90) == "1m30s", caudra.ui.humantime(90))
  assert(caudra.ui.humantime(3661) == "1h1m1s", caudra.ui.humantime(3661))
end)

case("highlight_keeps_the_source_text", function()
  local lines = caudra.ui.highlight("fn main() {}\nlet x = 1;", "rust")
  assert(#lines == 2, "line count " .. #lines)
  assert(lines_text(lines) == "fn main() {}\nlet x = 1;", lines_text(lines))
  assert(type(lines[1][1][2].fg) == "string", "spans carry a foreground")
end)

case("markdown_renders_headings_and_emphasis", function()
  local text = lines_text(caudra.ui.markdown("# Hello\n\nSome **bold** text.", 40))
  assert(text:find("Hello", 1, true), text)
  assert(text:find("Some bold text.", 1, true), text)
  assert(not text:find("**", 1, true), text)
end)

if #failures > 0 then
  error(#failures .. " case(s) failed:\n\n" .. table.concat(failures, "\n\n"))
end
