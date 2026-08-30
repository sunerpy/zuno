#[tokio::main]
async fn main() {
    eprintln!("zuno-acp conformance backend ready");
    let agent = zuno_acp::conformance::ConformanceAgent::new();
    if let Err(error) = zuno_acp::serve_stdio(agent).await {
        eprintln!("zuno-acp conformance backend failed: {error}");
        std::process::exit(1);
    }
}
