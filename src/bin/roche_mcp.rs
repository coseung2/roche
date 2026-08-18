fn main() {
    if let Err(message) = roche_workstation::mcp::run_stdio() {
        eprintln!("ROCHE_MCP_ERROR {message}");
        std::process::exit(1);
    }
}
