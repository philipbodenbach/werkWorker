# Tetris experiment

The `index.html` file is the original, unmodified result of a werkWorker / Werk1112
experiment, copied from `werkWorkerTest/index.html` on 2026-10-26. It runs in a
desktop browser with keyboard input, HTML Canvas and Web Audio. It has no runtime
dependency on werkWorker, external assets or a backend.

- [Play](https://philipbodenbach.github.io/werkWorker/experiments/tetris/)
- [Report, German](https://philipbodenbach.de/lab/werkworker-tetris/)
- [Report, English](https://philipbodenbach.de/en/lab/werkworker-tetris/)

The bilingual report, video, social previews and X drafts are maintained in the
`philipbodenbach.de` repository. This repository owns the playable artifact.

## Provenance

SHA-256 of the original and published copy:

```text
65cfee244a50c5b0bad7d0ca6692e66d31e08f095477a5dce2f015260829e90b
```

Session screenshots supplied by the author identify:

- Agent: werkWorker, `coding-basic` profile.
- Inference: Werk1112, llama.cpp / CUDA.
- Model identifier: `qwen3.8-27b-ud-q4_k_xl`.

The original full prompt and complete session log are not included. Session
counters and server uptime are not presented as performance benchmarks. No
`.werkworker` configuration, credentials or runtime data are published.

## Preserving the experiment

The original English game UI and paper aesthetic are part of the result. The
surrounding project and Lab pages support German/English and light/dark themes.
The game currently has a fixed playfield and keyboard controls; small screens
may clip it and touch controls are not implemented. Smoke checks do not establish
that all gameplay logic is correct. Preserve this file; put revised variants in
separate directories and document their changes.
