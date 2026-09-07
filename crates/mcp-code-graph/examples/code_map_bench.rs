//! Wall-clock harness for one `code_map` call over a real tree.
//!
//! Exists to be compared against the upstream `ripwire <dir>` binary this group was ported from,
//! whose default output is the same ranked map.
//!
//! Both numbers are needed for that comparison to be honest. `cold` builds a fresh group per run,
//! which is what the upstream binary does with its on-disk cache cleared. `warm` reuses one group,
//! which is what it does with that cache retained. Comparing our cold against its warm, or the
//! reverse, measures the cache and not the pipeline.
//!
//! ```text
//! cargo run --release --example code_map_bench -- <root> [runs]
//! ```

use std::time::Instant;
use tokio_util::sync::CancellationToken;
use workcell_mcp_code_graph::{CodeGraphToolGroup, CodeMapInput, ModelText};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let mut arguments = std::env::args().skip(1);
    let Some(root) = arguments.next() else {
        eprintln!("usage: code_map_bench <root> [runs]");
        std::process::exit(2);
    };
    let runs: usize = arguments
        .next()
        .map_or(1, |value| value.parse().expect("run count"));

    let token = CancellationToken::new();
    let mut cold = Vec::with_capacity(runs);
    let mut warm = Vec::with_capacity(runs);
    let mut rendered = 0;
    let mut summary = String::new();
    for _ in 0..runs {
        let started = Instant::now();
        let group = CodeGraphToolGroup::new(&root, None)
            .await
            .expect("build code graph group");
        let output = group
            .code_map(CodeMapInput::default(), &token)
            .await
            .expect("code_map");
        cold.push(started.elapsed());
        rendered = output.model_text().len();
        summary = format!("{:?}", output.graph);

        let started = Instant::now();
        group
            .code_map(CodeMapInput::default(), &token)
            .await
            .expect("code_map");
        warm.push(started.elapsed());
    }

    println!("root            {root}");
    println!("runs            {runs}");
    report("cold", &mut cold);
    report("warm", &mut warm);
    println!("graph           {summary}");
    println!("rendered bytes  {rendered}");
}

fn report(label: &str, elapsed: &mut [std::time::Duration]) {
    elapsed.sort_unstable();
    println!(
        "{label:<15} min {:.1} / median {:.1} / max {:.1} ms",
        elapsed[0].as_secs_f64() * 1e3,
        elapsed[elapsed.len() / 2].as_secs_f64() * 1e3,
        elapsed[elapsed.len() - 1].as_secs_f64() * 1e3
    );
}
