//! Run with cargo run --release -p globaltokentracker-core --example scheduler_bench.
//! Pure graph operations: no discovery, model generation, or production state.
use globaltokentracker_core::scheduler::{Edge, EdgeKind, Route, Workflow};
use std::hint::black_box;
use std::time::Instant;

fn main() {
    let mut graph = Workflow::new("benchmark".into());
    for _ in 0..256 {
        graph.add_route(&Route::default(), 20.0, 20.0).unwrap();
    }
    for b in 1usize..256 {
        for a in b.saturating_sub(17)..b {
            if graph.edges.len() < 4096 {
                graph.edges.push(Edge {
                    from: graph.nodes[a].id.clone(),
                    to: graph.nodes[b].id.clone(),
                    kind: EdgeKind::Task,
                });
            }
        }
    }
    assert_eq!(graph.order().unwrap().len(), 256);
    let start = Instant::now();
    for _ in 0..1000 {
        black_box(graph.order().unwrap());
    }
    let validation_us = start.elapsed().as_secs_f64() * 1000.0;
    let start = Instant::now();
    for i in 0..100_000 {
        // Alternate a topmost hit and a miss that scans every rectangle.
        black_box(black_box(&graph).hit(
            black_box(if i % 2 == 0 { 30.0 } else { 0.0 }),
            black_box(30.0),
        ));
    }
    let hit_ns = start.elapsed().as_secs_f64() * 10_000.0;
    println!(
        "{{\"nodes\":256,\"edges\":4096,\"validation_mean_us\":{validation_us:.3},\"hit_mean_ns\":{hit_ns:.3}}}"
    );
}
