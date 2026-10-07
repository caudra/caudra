"""Helper rules shared by the workcell build files."""

load("@bazel_skylib//lib:paths.bzl", "paths")

DEFAULT_COPTS = ["-Wall", "-Werror"]

WorkcellInfo = provider(
    doc = "Artifacts produced by a workcell rule.",
    fields = {"binary": "The built executable."},
)

def _workcell_impl(ctx):
    output = ctx.actions.declare_file(paths.basename(ctx.attr.entry))
    ctx.actions.run(
        outputs = [output],
        executable = ctx.executable._builder,
    )
    return [WorkcellInfo(binary = output)]

workcell_binary = rule(
    implementation = _workcell_impl,
    attrs = {"entry": attr.string()},
    executable = True,
)

def workcell_test_suite(name, shard_count = 1):
    """Declares the standard workcell test suite."""
    native.test_suite(
        name = name,
        tags = ["workcell"],
        shard_count = shard_count,
    )
