//! Wall-clock harness for one `code_map` call over a real tree.
//!
//! Exists to be compared against the upstream `ripwire <dir>` binary this group was ported from,
//! whose default output is the same ranked map.
//!
//! Each run builds a fresh group, so nothing is carried between runs and every file is extracted
//! from source. That is deliberate: the comparison this harness feeds measures ripwire **with** its
//! on-disk cache against this pipeline with **no** cache at all, because ripwire's cache survives a
//! process and ours does not. Crediting our in-process cache here would compare two things that are
//! not the same capability.
//!
//! The measured path still hashes every file, since the production ingest is the cached one and a
//! cold run is all misses. That overhead is left in rather than benchmarked away.
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
    let mut elapsed = Vec::with_capacity(runs);
    let mut rendered = 0;
    let mut summary = String::new();
    for _ in 0..runs {
        let started = Instant::now();
        let group = CodeGraphToolGroup::new(&root, None)
            .await
            .expect("build code graph group");
        let output = group
            .code_map(CodeMapInput::default(), None, &token)
            .await
            .expect("code_map");
        elapsed.push(started.elapsed());
        rendered = output.model_text().len();
        summary = format!("{:?}", output.graph);
    }

    elapsed.sort_unstable();
    println!("root            {root}");
    println!("runs            {runs}");
    println!(
        "uncached        min {:.1} / median {:.1} / max {:.1} ms",
        elapsed[0].as_secs_f64() * 1e3,
        elapsed[elapsed.len() / 2].as_secs_f64() * 1e3,
        elapsed[elapsed.len() - 1].as_secs_f64() * 1e3
    );
    println!("graph           {summary}");
    println!("rendered bytes  {rendered}");
}
