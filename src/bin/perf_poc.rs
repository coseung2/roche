use std::time::Instant;

use roche_workstation::perf::{
    STANDARD_TERMINAL_BYTES, SyntheticWorkload, TerminalRingBuffer, VirtualWindow,
};

fn main() {
    println!("Roche Phase 0A performance core");
    println!("================================");

    let started = Instant::now();
    let workload = SyntheticWorkload::standard();
    let workload_elapsed = started.elapsed();

    let conversation_window = VirtualWindow::around(workload.messages.len(), 50_000, 40, 20);
    let tool_window = VirtualWindow::around(workload.tool_events.len(), 50_000, 40, 20);
    let session_window = VirtualWindow::around(workload.sessions.len(), 500, 30, 10);

    let streaming_started = Instant::now();
    let mut max_materialized = 0;
    for total_items in 1..=workload.messages.len() {
        max_materialized = max_materialized.max(VirtualWindow::tail(total_items, 40, 20).len());
    }
    let streaming_elapsed = streaming_started.elapsed();

    let terminal_started = Instant::now();
    let mut terminal = TerminalRingBuffer::new(500);
    terminal.ingest_repeated_line_to_bytes(
        "[codex] compiling synthetic workspace target; output remains bounded in the UI ring buffer",
        STANDARD_TERMINAL_BYTES,
    );
    let terminal_elapsed = terminal_started.elapsed();

    println!(
        "workload: messages={} tool_events={} sessions={} build_ms={}",
        workload.messages.len(),
        workload.tool_events.len(),
        workload.sessions.len(),
        workload_elapsed.as_millis()
    );
    println!(
        "conversation_window: materialized={} total={}",
        conversation_window.len(),
        workload.messages.len()
    );
    println!(
        "tool_window: materialized={} total={}",
        tool_window.len(),
        workload.tool_events.len()
    );
    println!(
        "session_window: materialized={} total={}",
        session_window.len(),
        workload.sessions.len()
    );
    println!(
        "streaming_tail: appends={} max_materialized={} scan_ms={}",
        workload.messages.len(),
        max_materialized,
        streaming_elapsed.as_millis()
    );
    println!(
        "terminal: ingested_bytes={} resident_lines={} resident_bytes={} ingest_ms={}",
        terminal.total_bytes_ingested(),
        terminal.resident_lines(),
        terminal.resident_bytes(),
        terminal_elapsed.as_millis()
    );
}
