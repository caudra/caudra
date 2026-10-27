#[path = "../../tests/embedded_worker.rs"]
mod embedded_worker;

fn main() {
    embedded_worker::production_host_computes_through_the_code_tool_with_the_embedded_worker();
    embedded_worker::production_host_runs_mcp_snippets_with_the_embedded_worker();
    embedded_worker::production_host_prints_an_answer_as_its_repr();
}
