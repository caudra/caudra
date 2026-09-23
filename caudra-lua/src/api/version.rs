use caudra_lua_macro::lua_fn;
use mlua::{Lua, Result as LuaResult, Table};

const MAJOR: u32 = version_component(env!("CARGO_PKG_VERSION_MAJOR"));
const MINOR: u32 = version_component(env!("CARGO_PKG_VERSION_MINOR"));
const PATCH: u32 = version_component(env!("CARGO_PKG_VERSION_PATCH"));
const PRERELEASE: &str = env!("CARGO_PKG_VERSION_PRE");

/// Evaluated only in const context, so a malformed component fails the build.
const fn version_component(digits: &str) -> u32 {
    match u32::from_str_radix(digits, 10) {
        Ok(component) => component,
        Err(_) => panic!("Cargo package version components are decimal integers"),
    }
}

/// Return the version of the running Caudra build. Mirrors Neovim's
/// `vim.version()`, so one `init.lua` can adapt to several releases instead
/// of declaring a config version.
///
/// @return (table) `major`, `minor` and `patch` integers, plus a `prerelease`
/// string on pre-release builds.
/// @example
/// local v = caudra.version()
/// if v.major > 0 or v.minor >= 2 then
///   -- use a setting added in 0.2
/// end
#[lua_fn]
fn version(lua: &Lua) -> LuaResult<Table> {
    let version = lua.create_table_from([("major", MAJOR), ("minor", MINOR), ("patch", PATCH)])?;
    if !PRERELEASE.is_empty() {
        version.set("prerelease", PRERELEASE)?;
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RENDER_VERSION: &str = r#"
        local v = caudra.version()
        local core = ("%d.%d.%d"):format(v.major, v.minor, v.patch)
        return v.prerelease and (core .. "-" .. v.prerelease) or core
    "#;

    #[test]
    fn version_recomposes_the_package_version() {
        let lua = Lua::new();
        let caudra = lua.create_table().unwrap();
        version__register(&caudra, &lua).unwrap();
        lua.globals().set("caudra", caudra).unwrap();

        let rendered: String = lua.load(RENDER_VERSION).eval().unwrap();

        assert_eq!(rendered, env!("CARGO_PKG_VERSION"));
    }
}
