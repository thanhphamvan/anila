//! Terminal workload recorder and codec/framing and network benchmark.

fn main() -> anyhow::Result<()> {
    anila_e2e::bench::cli(&std::env::args().skip(1).collect::<Vec<_>>())
}
