//! The JavaScript library calls the translator maps, as data. A row whose Python takes the same
//! arguments is a [`Form`] rename; any row that reshapes its arguments names a [`Shape`], which
//! `expressions` builds in one exhaustive match. A call no row names refuses.

use std::ops::RangeInclusive;

use super::expressions::Fact;
use super::python::{Builtin, Import};

const ANY: usize = usize::MAX;

pub(super) struct Mapping {
    /// `console.log` for a global, `map` for a method.
    pub(super) written: &'static str,
    pub(super) arity: RangeInclusive<usize>,
    pub(super) form: Form,
    /// The single source of what the result is known to be, which truthiness reads.
    pub(super) returns: Fact,
    /// A snippet using the row, and the Python it translates to.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "only the test of every row reads it")
    )]
    pub(super) example: (&'static str, &'static str),
}

#[derive(Clone, Copy)]
pub(super) enum Form {
    /// The same arguments to a builtin.
    Builtin(Builtin),
    /// The same arguments to a module's function.
    Imported(Import, &'static str),
    /// The same arguments to the receiver's own Python method.
    Method(&'static str),
    Shaped(Shape),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Shape {
    /// `JSON.stringify(x)` as compact `json.dumps(x)`, and `JSON.stringify(x, null, n)` as
    /// `json.dumps(x, indent=n)`, both keeping non-ASCII text as JavaScript does.
    Dumps,
    /// Two or more whole numbers, as they are: Python's `max` of a list, a string or NaN differs.
    Extremum(Builtin),
    /// `Object.values(o)` as `list(o.values())`.
    Values,
    /// `Object.entries(o)` as `list(o.items())`.
    Entries,
    /// `Object.assign({}, a, b)` as `{**a, **b}`.
    Assign,
    /// `Array.isArray(x)` as `isinstance(x, list)`.
    IsArray,
    /// `parseInt(x, 10)` as `int(x)`.
    ParseInt,
    /// `Promise.all(...)` as `asyncio.gather(...)`.
    Gather,
    Map,
    Filter,
    Find,
    Some,
    Every,
    FlatMap,
    /// Statement position only, as a `for` loop.
    ForEach,
    /// Statement position only, as `append`.
    Push,
    Includes,
    Slice,
    Join,
    Concat,
    At,
    Replace,
    ReplaceAll,
    /// `s.split(",")` as it is, and `s.split("")` as `list(s)`.
    Split,
    /// `s.trim()` as the named `strip`, given JavaScript's whitespace, which differs from Python's.
    Trim(&'static str),
    Substring,
}

pub(super) const GLOBALS: [Mapping; 21] = [
    Mapping {
        written: "console.log",
        arity: 0..=ANY,
        form: Form::Builtin(Builtin::Print),
        returns: Fact::Unknown,
        example: (
            "const a = 1;\nconsole.log(\"a\", a);",
            "a = 1\nprint(\"a\", a)",
        ),
    },
    Mapping {
        written: "console.info",
        arity: 0..=ANY,
        form: Form::Builtin(Builtin::Print),
        returns: Fact::Unknown,
        example: (
            "const xs = [1];\nconsole.info(...xs);",
            "xs = [1]\nprint(*xs)",
        ),
    },
    Mapping {
        written: "console.warn",
        arity: 0..=ANY,
        form: Form::Builtin(Builtin::Print),
        returns: Fact::Unknown,
        example: ("console.warn(`a`);", "print(\"a\")"),
    },
    Mapping {
        written: "console.error",
        arity: 0..=ANY,
        form: Form::Builtin(Builtin::Print),
        returns: Fact::Unknown,
        example: ("console.error(`a`);", "print(\"a\")"),
    },
    Mapping {
        written: "JSON.stringify",
        arity: 1..=3,
        form: Form::Shaped(Shape::Dumps),
        returns: Fact::Str,
        example: (
            "const o = {a: 1};\nJSON.stringify(o, null, 2);",
            "import json\no = {\"a\": 1}\njson.dumps(o, indent=2, ensure_ascii=False)",
        ),
    },
    Mapping {
        written: "JSON.parse",
        arity: 1..=1,
        form: Form::Imported(Import::Json, "loads"),
        returns: Fact::Unknown,
        example: (
            "const o = JSON.parse(\"{}\");",
            "import json\no = json.loads(\"{}\")",
        ),
    },
    Mapping {
        written: "Math.max",
        arity: 1..=ANY,
        form: Form::Shaped(Shape::Extremum(Builtin::Max)),
        returns: Fact::Integer,
        example: (
            "const xs = [1];\nMath.max(xs.length, 1);",
            "xs = [1]\nmax(len(xs), 1)",
        ),
    },
    Mapping {
        written: "Math.min",
        arity: 1..=ANY,
        form: Form::Shaped(Shape::Extremum(Builtin::Min)),
        returns: Fact::Integer,
        example: ("const a = 1;\nMath.min(a, 2);", "a = 1\nmin(a, 2)"),
    },
    Mapping {
        written: "Math.abs",
        arity: 1..=1,
        form: Form::Builtin(Builtin::Abs),
        returns: Fact::Number,
        example: ("const a = -1;\nMath.abs(a);", "a = -1\nabs(a)"),
    },
    Mapping {
        written: "Math.floor",
        arity: 1..=1,
        form: Form::Imported(Import::Math, "floor"),
        returns: Fact::Number,
        example: (
            "const a = 1.5;\nMath.floor(a);",
            "import math\na = 1.5\nmath.floor(a)",
        ),
    },
    Mapping {
        written: "Math.ceil",
        arity: 1..=1,
        form: Form::Imported(Import::Math, "ceil"),
        returns: Fact::Number,
        example: (
            "const a = 1.5;\nMath.ceil(a);",
            "import math\na = 1.5\nmath.ceil(a)",
        ),
    },
    Mapping {
        written: "Object.keys",
        arity: 1..=1,
        form: Form::Builtin(Builtin::List),
        returns: Fact::Unknown,
        example: ("const o = {};\nObject.keys(o);", "o = {}\nlist(o)"),
    },
    Mapping {
        written: "Object.values",
        arity: 1..=1,
        form: Form::Shaped(Shape::Values),
        returns: Fact::Unknown,
        example: (
            "const o = {};\nObject.values(o);",
            "o = {}\nlist(o.values())",
        ),
    },
    Mapping {
        written: "Object.entries",
        arity: 1..=1,
        form: Form::Shaped(Shape::Entries),
        returns: Fact::Unknown,
        example: (
            "const o = {};\nObject.entries(o);",
            "o = {}\nlist(o.items())",
        ),
    },
    Mapping {
        written: "Object.fromEntries",
        arity: 1..=1,
        form: Form::Builtin(Builtin::Dict),
        returns: Fact::Unknown,
        example: (
            "const p = [[\"a\", 1]];\nObject.fromEntries(p);",
            "p = [[\"a\", 1]]\ndict(p)",
        ),
    },
    Mapping {
        written: "Object.assign",
        arity: 2..=ANY,
        form: Form::Shaped(Shape::Assign),
        returns: Fact::Unknown,
        example: (
            "const a = {};\nconst b = {};\nObject.assign({}, a, b);",
            "a = {}\nb = {}\n{**a, **b}",
        ),
    },
    Mapping {
        written: "Array.isArray",
        arity: 1..=1,
        form: Form::Shaped(Shape::IsArray),
        returns: Fact::Bool,
        example: (
            "const a = [];\nArray.isArray(a);",
            "a = []\nisinstance(a, list)",
        ),
    },
    Mapping {
        written: "String",
        arity: 1..=1,
        form: Form::Builtin(Builtin::Str),
        returns: Fact::Str,
        example: ("const a = 1;\nString(a);", "a = 1\nstr(a)"),
    },
    Mapping {
        written: "Number",
        arity: 1..=1,
        form: Form::Builtin(Builtin::Float),
        returns: Fact::Number,
        example: ("const a = \"1\";\nNumber(a);", "a = \"1\"\nfloat(a)"),
    },
    Mapping {
        written: "parseFloat",
        arity: 1..=1,
        form: Form::Builtin(Builtin::Float),
        returns: Fact::Number,
        example: (
            "const a = \"1.5\";\nparseFloat(a);",
            "a = \"1.5\"\nfloat(a)",
        ),
    },
    Mapping {
        written: "parseInt",
        arity: 1..=2,
        form: Form::Shaped(Shape::ParseInt),
        returns: Fact::Number,
        example: ("const a = \"1\";\nparseInt(a, 10);", "a = \"1\"\nint(a)"),
    },
];

/// `Promise.all`, apart from [`GLOBALS`] because its example needs a host.
pub(super) const GATHER: Mapping = Mapping {
    written: "Promise.all",
    arity: 1..=1,
    form: Form::Shaped(Shape::Gather),
    returns: Fact::Unknown,
    example: (
        "const [a, b] = await Promise.all([gh.list(), gh.list()]);",
        "import asyncio\na, b = await asyncio.gather(gh.list(), gh.list())",
    ),
};

pub(super) const METHODS: [Mapping; 24] = [
    Mapping {
        written: "map",
        arity: 1..=1,
        form: Form::Shaped(Shape::Map),
        returns: Fact::Unknown,
        example: (
            "const xs = [1];\nxs.map((x, i) => [x, i]);",
            "xs = [1]\n[[x, i] for i, x in enumerate(xs)]",
        ),
    },
    Mapping {
        written: "filter",
        arity: 1..=1,
        form: Form::Shaped(Shape::Filter),
        returns: Fact::Unknown,
        example: (
            "const xs = [\"a\"];\nxs.filter(x => x.length > 0);",
            "xs = [\"a\"]\n[x for x in xs if len(x) > 0]",
        ),
    },
    Mapping {
        written: "find",
        arity: 1..=1,
        form: Form::Shaped(Shape::Find),
        returns: Fact::Optional,
        example: (
            "const xs = [\"a\"];\nxs.find(x => x.length > 0);",
            "xs = [\"a\"]\nnext((x for x in xs if len(x) > 0), None)",
        ),
    },
    Mapping {
        written: "some",
        arity: 1..=1,
        form: Form::Shaped(Shape::Some),
        returns: Fact::Bool,
        example: (
            "const xs = [\"a\"];\nxs.some(x => x.length > 0);",
            "xs = [\"a\"]\nany(len(x) > 0 for x in xs)",
        ),
    },
    Mapping {
        written: "every",
        arity: 1..=1,
        form: Form::Shaped(Shape::Every),
        returns: Fact::Bool,
        example: (
            "const xs = [\"a\"];\nxs.every(x => x.length > 0);",
            "xs = [\"a\"]\nall(len(x) > 0 for x in xs)",
        ),
    },
    Mapping {
        written: "flatMap",
        arity: 1..=1,
        form: Form::Shaped(Shape::FlatMap),
        returns: Fact::Unknown,
        example: (
            "const xs = [1];\nxs.flatMap(x => [x, x]);",
            "xs = [1]\n[_caudra_item for x in xs for _caudra_item in [x, x]]",
        ),
    },
    Mapping {
        written: "forEach",
        arity: 1..=1,
        form: Form::Shaped(Shape::ForEach),
        returns: Fact::Unknown,
        example: (
            "const xs = [1];\nxs.forEach(x => console.log(x));",
            "xs = [1]\nfor x in xs:\n    print(x)",
        ),
    },
    Mapping {
        written: "push",
        arity: 1..=1,
        form: Form::Shaped(Shape::Push),
        returns: Fact::Unknown,
        example: ("const xs = [];\nxs.push(1);", "xs = []\nxs.append(1)"),
    },
    Mapping {
        written: "includes",
        arity: 1..=1,
        form: Form::Shaped(Shape::Includes),
        returns: Fact::Bool,
        example: (
            "const xs = [\"a\"];\nxs.includes(\"a\");",
            "xs = [\"a\"]\n\"a\" in xs",
        ),
    },
    Mapping {
        written: "slice",
        arity: 0..=2,
        form: Form::Shaped(Shape::Slice),
        returns: Fact::Unknown,
        example: ("const xs = [1];\nxs.slice(1, -1);", "xs = [1]\nxs[1:-1]"),
    },
    Mapping {
        written: "join",
        arity: 0..=1,
        form: Form::Shaped(Shape::Join),
        returns: Fact::Str,
        example: (
            "const xs = [1];\nxs.join(\", \");",
            "xs = [1]\n\", \".join(str(_caudra_item) for _caudra_item in xs)",
        ),
    },
    Mapping {
        written: "concat",
        arity: 1..=ANY,
        form: Form::Shaped(Shape::Concat),
        returns: Fact::Unknown,
        example: (
            "const xs = [1];\nxs.concat(xs, [2]);",
            "xs = [1]\nxs + xs + [2]",
        ),
    },
    Mapping {
        written: "at",
        arity: 1..=1,
        form: Form::Shaped(Shape::At),
        returns: Fact::Unknown,
        example: ("const xs = [1];\nxs.at(-1);", "xs = [1]\nxs[-1]"),
    },
    Mapping {
        written: "toUpperCase",
        arity: 0..=0,
        form: Form::Method("upper"),
        returns: Fact::Str,
        example: ("const s = \"a\";\ns.toUpperCase();", "s = \"a\"\ns.upper()"),
    },
    Mapping {
        written: "toLowerCase",
        arity: 0..=0,
        form: Form::Method("lower"),
        returns: Fact::Str,
        example: ("const s = \"a\";\ns.toLowerCase();", "s = \"a\"\ns.lower()"),
    },
    Mapping {
        written: "trim",
        arity: 0..=0,
        form: Form::Shaped(Shape::Trim("strip")),
        returns: Fact::Str,
        example: (
            "const s = \"a\";\ns.trim();",
            "s = \"a\"\ns.strip(\"\\t\\n\\x0b\\x0c\\r \\xa0\\u1680\\u2000\\u2001\\u2002\\u2003\\u2004\\u2005\\u2006\\u2007\\u2008\\u2009\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000\\ufeff\")",
        ),
    },
    Mapping {
        written: "trimStart",
        arity: 0..=0,
        form: Form::Shaped(Shape::Trim("lstrip")),
        returns: Fact::Str,
        example: (
            "const s = \"a\";\ns.trimStart();",
            "s = \"a\"\ns.lstrip(\"\\t\\n\\x0b\\x0c\\r \\xa0\\u1680\\u2000\\u2001\\u2002\\u2003\\u2004\\u2005\\u2006\\u2007\\u2008\\u2009\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000\\ufeff\")",
        ),
    },
    Mapping {
        written: "trimEnd",
        arity: 0..=0,
        form: Form::Shaped(Shape::Trim("rstrip")),
        returns: Fact::Str,
        example: (
            "const s = \"a\";\ns.trimEnd();",
            "s = \"a\"\ns.rstrip(\"\\t\\n\\x0b\\x0c\\r \\xa0\\u1680\\u2000\\u2001\\u2002\\u2003\\u2004\\u2005\\u2006\\u2007\\u2008\\u2009\\u200a\\u2028\\u2029\\u202f\\u205f\\u3000\\ufeff\")",
        ),
    },
    Mapping {
        written: "startsWith",
        arity: 1..=1,
        form: Form::Method("startswith"),
        returns: Fact::Bool,
        example: (
            "const s = \"a\";\ns.startsWith(\"a\");",
            "s = \"a\"\ns.startswith(\"a\")",
        ),
    },
    Mapping {
        written: "endsWith",
        arity: 1..=1,
        form: Form::Method("endswith"),
        returns: Fact::Bool,
        example: (
            "const s = \"a\";\ns.endsWith(\"a\");",
            "s = \"a\"\ns.endswith(\"a\")",
        ),
    },
    Mapping {
        written: "split",
        arity: 1..=1,
        form: Form::Shaped(Shape::Split),
        returns: Fact::Unknown,
        example: (
            "const s = \"a,b\";\ns.split(\",\");",
            "s = \"a,b\"\ns.split(\",\")",
        ),
    },
    Mapping {
        written: "replace",
        arity: 2..=2,
        form: Form::Shaped(Shape::Replace),
        returns: Fact::Str,
        example: (
            "const s = \"a\";\ns.replace(\"a\", \"b\");",
            "s = \"a\"\ns.replace(\"a\", \"b\", 1)",
        ),
    },
    Mapping {
        written: "replaceAll",
        arity: 2..=2,
        form: Form::Shaped(Shape::ReplaceAll),
        returns: Fact::Str,
        example: (
            "const s = \"a\";\ns.replaceAll(\"a\", \"b\");",
            "s = \"a\"\ns.replace(\"a\", \"b\")",
        ),
    },
    Mapping {
        written: "substring",
        arity: 1..=2,
        form: Form::Shaped(Shape::Substring),
        returns: Fact::Str,
        example: (
            "const s = \"abc\";\ns.substring(1, 2);",
            "s = \"abc\"\ns[1:2]",
        ),
    },
];

pub(super) fn global(written: &str) -> Option<&'static Mapping> {
    GLOBALS
        .iter()
        .chain([&GATHER])
        .find(|mapping| mapping.written == written)
}

/// Whether `name` is a global object some row calls into, as `Math` or `Promise`.
pub(super) fn is_global_object(name: &str) -> bool {
    GLOBALS.iter().chain([&GATHER]).any(|mapping| {
        mapping
            .written
            .split_once('.')
            .is_some_and(|(object, _)| object == name)
    })
}

pub(super) fn method(written: &str) -> Option<&'static Mapping> {
    METHODS.iter().find(|mapping| mapping.written == written)
}

#[cfg(test)]
mod tests {
    use super::super::tests::python;
    use super::{GATHER, GLOBALS, METHODS};

    #[test]
    fn every_mapping_translates_its_example() {
        for mapping in GLOBALS.iter().chain([&GATHER]).chain(&METHODS) {
            let (javascript, expected) = mapping.example;
            assert_eq!(python(javascript), expected, "{}", mapping.written);
        }
    }
}
