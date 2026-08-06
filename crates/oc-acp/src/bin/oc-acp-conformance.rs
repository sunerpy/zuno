#[tokio::main]
async fn main() {
    eprintln!("oc-acp conformance backend ready");
    let agent = oc_acp::conformance::ConformanceAgent::new();
    if let Err(error) = oc_acp::serve_stdio(agent).await {
        eprintln!("oc-acp conformance backend failed: {error}");
        std::process::exit(1);
    }
}
