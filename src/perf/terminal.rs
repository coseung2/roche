use std::collections::VecDeque;

#[derive(Debug, Clone)]
pub struct TerminalRingBuffer {
    capacity_lines: usize,
    lines: VecDeque<String>,
    pending: String,
    total_bytes_ingested: u64,
    total_lines_ingested: u64,
    resident_bytes: usize,
}

impl TerminalRingBuffer {
    pub fn new(capacity_lines: usize) -> Self {
        Self {
            capacity_lines,
            lines: VecDeque::with_capacity(capacity_lines),
            pending: String::new(),
            total_bytes_ingested: 0,
            total_lines_ingested: 0,
            resident_bytes: 0,
        }
    }

    pub fn push_chunk(&mut self, chunk: &str) {
        self.total_bytes_ingested = self.total_bytes_ingested.saturating_add(chunk.len() as u64);

        let mut start = 0;
        for (newline_index, _) in chunk.match_indices('\n') {
            let segment = &chunk[start..newline_index];
            if self.pending.is_empty() {
                self.record_line(segment.to_owned());
            } else {
                self.pending.push_str(segment);
                let line = std::mem::take(&mut self.pending);
                self.record_line(line);
            }
            start = newline_index + 1;
        }

        self.pending.push_str(&chunk[start..]);
    }

    pub fn push_complete_line(&mut self, line: &str) {
        self.total_bytes_ingested = self
            .total_bytes_ingested
            .saturating_add(line.len() as u64 + 1);

        if self.pending.is_empty() {
            self.record_line(line.to_owned());
        } else {
            self.pending.push_str(line);
            let complete = std::mem::take(&mut self.pending);
            self.record_line(complete);
        }
    }

    pub fn ingest_repeated_line_to_bytes(&mut self, line: &str, target_total_bytes: u64) {
        while self.total_bytes_ingested < target_total_bytes {
            self.push_complete_line(line);
        }
    }

    pub fn flush_partial_line(&mut self) {
        if self.pending.is_empty() {
            return;
        }

        let line = std::mem::take(&mut self.pending);
        self.record_line(line);
    }

    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(String::as_str)
    }

    pub fn resident_lines(&self) -> usize {
        self.lines.len()
    }

    pub fn resident_bytes(&self) -> usize {
        self.resident_bytes + self.pending.len()
    }

    pub fn total_bytes_ingested(&self) -> u64 {
        self.total_bytes_ingested
    }

    pub fn total_lines_ingested(&self) -> u64 {
        self.total_lines_ingested
    }

    fn record_line(&mut self, line: String) {
        self.total_lines_ingested = self.total_lines_ingested.saturating_add(1);

        if self.capacity_lines == 0 {
            return;
        }

        if self.lines.len() == self.capacity_lines
            && let Some(evicted) = self.lines.pop_front()
        {
            self.resident_bytes = self.resident_bytes.saturating_sub(evicted.len());
        }

        self.resident_bytes = self.resident_bytes.saturating_add(line.len());
        self.lines.push_back(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perf::STANDARD_TERMINAL_BYTES;

    #[test]
    fn chunk_boundaries_preserve_partial_lines() {
        let mut buffer = TerminalRingBuffer::new(10);
        buffer.push_chunk("first\nsec");
        buffer.push_chunk("ond\nthird");
        buffer.flush_partial_line();

        assert_eq!(
            buffer.lines().collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert_eq!(buffer.total_lines_ingested(), 3);
    }

    #[test]
    fn fifty_megabyte_history_keeps_only_recent_lines_resident() {
        let mut buffer = TerminalRingBuffer::new(500);
        let line = "[codex] compiling synthetic workspace target; output remains bounded in the UI ring buffer";

        buffer.ingest_repeated_line_to_bytes(line, STANDARD_TERMINAL_BYTES);

        assert!(buffer.total_bytes_ingested() >= STANDARD_TERMINAL_BYTES);
        assert_eq!(buffer.resident_lines(), 500);
        assert!(buffer.resident_bytes() < 64 * 1024);
        assert!(buffer.total_lines_ingested() > buffer.resident_lines() as u64);
    }
}
