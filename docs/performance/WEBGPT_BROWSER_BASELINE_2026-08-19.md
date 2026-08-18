# Web GPT Browser Runtime Baseline — 2026-08-19

This is the M0 single-slot resource baseline for the current production-disconnected `SharedWebGptBrowser` path. It is not a multi-slot benchmark and does not establish a final concurrency limit.

## Conditions

- OS: Windows development workstation
- Logical processors: 12
- Binary: `target\debug\roche-workstation.exe`, built after the latest source timestamp
- Browser topology: one hidden WebView2 helper and the current single global FIFO slot
- Profile: temporary clean `LOCALAPPDATA`; not the user's authenticated Web GPT profile
- Runtime state: `codex_ready=true`, `codex_busy=false`, no Task/chat/worker turns
- Web GPT state: login unavailable in the clean profile, therefore `web_worker_ready=false`
- Sampling: one three-second idle CPU sample after bridge readiness
- External actions: no model turn, account login, tunnel setup or app enrollment

The temporary app closed through its native window path without force, and its temporary profile directory and stopped-process descriptor were removed afterward.

## Process sample

| Process group | Count | Notable working set |
| --- | ---: | ---: |
| Roche main process | 1 | 122.9 MiB |
| Roche WebView2 helper | 1 | 30.5 MiB |
| WebView2 subprocesses | 6 | 584.2 MiB combined |
| Codex/Node/console descendants | 4 | 154.5 MiB combined |
| Full descendant tree | 12 | 892.1 MiB combined |

The full tree reported 606.7 MiB combined private bytes. Summed working sets can double-count shared pages, so private bytes and per-process trends should be used alongside the aggregate working-set number when comparing future runs.

Observed whole-machine-normalized CPU was low but non-zero while idle. The main process, helper and largest renderer accounted for most sampled activity; this one short sample is diagnostic evidence, not a stable CPU benchmark.

## Current fixed-cost sources

- One `WebContext` and one hidden WebView are created eagerly.
- `BackgroundThrottlingPolicy::Disabled` keeps the hidden page active.
- A dedicated probe thread sends `BrowserCommand::Probe` every 500 ms.
- Every probe evaluates both login and chat-state scripts even when no turn is active.
- `SharedBrowserInner` has one `active_request_id`; all remaining turns wait in `pending_turns`.

These facts mean a naive clone of the current WebView and 500 ms probe loop could multiply renderer memory and idle script evaluation. M1 must therefore use lazy slot creation and adaptive probing instead of one permanent probe thread per slot.

## Wry capability evidence

The installed Wry 0.55.1 source documents `WebViewBuilder::new_with_web_context` as creating a builder with a `WebContext` that can be shared with multiple WebViews. Its Windows warning also requires different data directories when `CoreWebView2EnvironmentOptions` differ.

This establishes API-level feasibility. Roche also ran the production-disconnected `webgpt_multi_webview_smoke` against the installed WebView2 runtime with one helper event loop, one temporary profile and two child WebViews. Both views completed ChatGPT navigation and independent JavaScript probes before the process exited normally:

```text
WEBGPT_MULTI_WEBVIEW_READY slots=2
```

The smoke sent no prompt or model turn and removed its temporary profile afterward. This proves two-view creation/navigation/probing feasibility, but not authenticated same-account concurrent generation, cancellation or long-running stability. Different ChatGPT accounts remain separate profile/data-directory owners regardless of same-account sharing support.

### Two-view post-navigation sample

The same smoke was repeated with an eight-second hold after both probes completed. A two-second sample during that hold reported:

| Process group | Count | Working set | Private bytes |
| --- | ---: | ---: | ---: |
| Smoke host | 1 | 25.2 MiB | 4.3 MiB |
| WebView2 subprocesses | 7 | 916.1 MiB | 700.6 MiB |
| Console host | 1 | 12.6 MiB | 2.3 MiB |
| Full smoke tree | 9 | 953.9 MiB | 707.2 MiB |

The short post-navigation CPU sample was 13.3% when normalized across 12 logical processors. It is not an idle steady-state result because the pages had just loaded. For rough context, the first one-view full-app sample attributed about 614.7 MiB working set and 428.7 MiB private bytes to the browser helper plus WebView2 subtree, but the two measurements use different host process topologies and timing. Treat the apparent delta as a capacity warning, not a precise per-slot cost.

The temporary smoke process exited and its profile was deleted after measurement.

## Preflight finding

Before the controlled run, `.ai-bridge/roche-webgpt-runtime.json` referenced a dead PID and `roche_web health` timed out. This confirms stale descriptor handling remains a real recovery requirement. M0/M6 tests should distinguish a stopped owner from a busy live bridge and must never attach to a descriptor solely because the file exists.

## Comparison contract for M1

Repeat the same measurement for:

1. one idle logged-out slot,
2. one idle authenticated slot,
3. two created but idle slots,
4. one active plus one idle slot,
5. two active slots,
6. slot teardown and recreation.

Record main/helper/renderer private bytes, working set, CPU, readiness latency and probe cadence. Default two-slot concurrency is acceptable only after independent request-correlation tests pass and the measured resource delta is understood. Three slots remain opt-in until separately validated.

Run the non-model multi-view probe with:

```powershell
$env:ROCHE_WEBGPT_MULTI_WEBVIEW_PROFILE = Join-Path $env:TEMP "roche-multi-webview-smoke"
cargo run --bin webgpt_multi_webview_smoke
```
