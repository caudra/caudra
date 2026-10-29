use std::sync::Arc;

use caudra_agent::tools::ToolRegistry;
use caudra_lua::PluginHost;
use test_case::test_case;

#[test_case("api", include_str!("lua/api_spec.lua") ; "api_spec")]
#[test_case("lib", include_str!("lua/lib_spec.lua") ; "lib_spec")]
#[test_case("view_image", include_str!("lua/view_image_spec.lua") ; "view_image_spec")]
fn plugin_spec(name: &str, spec: &str) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source(&format!("{name}_spec"), spec)
        .unwrap_or_else(|e| panic!("{name} spec failed:\n{e}"));
}
