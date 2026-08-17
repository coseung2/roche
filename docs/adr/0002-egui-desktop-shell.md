# ADR-0002: Use egui/eframe for the first native desktop shell

- Status: Accepted for Phase 0B
- Date: 2026-08-18

## Context

Roche needs a Windows-native shell that can be exercised before Herdr integration is complete. The shell must render large session, conversation, tool, and terminal histories without creating widgets for the full history.

The Phase 0A core already proves bounded materialization independently of a GUI framework. Phase 0B needs a real window that can consume the same workload and preserve that property.

## Decision

Use `egui` through `eframe` 0.35 for the first desktop shell.

The decisive capability for this phase is `ScrollArea::show_rows`, which computes and renders only the visible range for large fixed-height lists. The shell uses that API for:

- 100,000 conversation rows
- 100,000 tool-event rows
- 1,000 session rows
- the resident terminal ring buffer

The dependency uses the native `glow` renderer with default fonts and persistence support. This choice is deliberately scoped to the first usable desktop shell and does not prevent replacement if later IME, text-selection, Markdown, terminal, or profiling tests expose unacceptable limits.

## Consequences

### Positive

- A real Windows desktop app can be built immediately on top of the existing Rust domain/performance core.
- Virtualized list behavior maps directly to a framework primitive instead of a custom scroll implementation.
- The UI remains a projection over domain/runtime state; Herdr integration can replace synthetic data without rewriting the shell structure.

### Negative

- The first shell uses fixed-height rows for the large virtualized views.
- Rich Markdown with variable-height layout is not yet benchmarked.
- Windows IME, large text selection, accessibility, and terminal-emulation behavior still need explicit validation.
- This ADR does not claim that egui is the permanent framework choice.

## Validation

Before this ADR is promoted from Phase 0B acceptance to a long-term framework decision, Roche must validate:

1. 100k conversation scrolling and jump-to-bottom behavior.
2. Streaming append without full-history layout.
3. Windows Korean IME input.
4. Text selection/copy over long content.
5. Markdown/code-block rendering and cache strategy.
6. Long-running memory behavior.
7. Terminal integration requirements.

If those checks fail materially, create a superseding ADR comparing the failed requirement against iced, Slint, or another viable native framework.
