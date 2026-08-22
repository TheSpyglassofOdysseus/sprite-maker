# Sprite Studio Baseline Test

Purpose: establish a clean, evidence-based baseline of the newly synced upstream Sprite Studio before adding fork-specific features.

This is a deliberately small side project. Do not redesign the application or invent features during this pass. First determine what works, what fails, and where the highest-value friction actually is.

## 1. Baseline and environment

Start from this branch. Record:

- operating system and architecture
- Bun version
- Rust and Cargo versions
- Node version if present
- Tauri/native prerequisites available or missing

Install declared dependencies where reasonable instead of treating them as blockers. Do not alter product behavior during setup.

## 2. Automated verification

Run, in order:

```bash
bun install
bun run check
bun test
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

Then attempt the appropriate development/build command for the current machine:

```bash
bun tauri dev
```

If a GUI cannot be launched in the current environment, do not fake that portion of testing. Complete all headless verification and clearly identify the remaining desktop-only checks.

## 3. Functional smoke test

Where GUI access is available, test the current application without modifying it first:

1. Create or open a project.
2. Confirm provider discovery and chat startup.
3. Generate one static sprite.
4. Generate or inspect an asset pack.
5. Select an existing asset and make an `animate this` style request.
6. Verify animation playback and FPS behavior.
7. Open the Rig workflow and test deterministic rigging if available.
8. Inspect a sprite with zoom/viewer controls.
9. Create a sprite-sheet export.
10. Exercise terrain generation/export if supported by the current UI.
11. Create a project backup and verify restore/import behavior.
12. Restart the application and confirm project, chat, and artifact persistence.

## 4. Report before improving

Post the baseline results to the PR using exactly these sections:

### WORKS WELL

Observed behavior that is solid enough to preserve.

### BUGS / FRICTION

Observed failures, confusing behavior, setup problems, poor UX, or reliability concerns. Include reproduction details where practical.

### HIGH-VALUE IMPROVEMENTS

Only propose improvements tied to something observed during this baseline. Separate likely upstream bugs from ideas that are specifically useful for this fork.

Do not implement feature improvements during the baseline pass unless a tiny setup-only correction is required to make the documented project build as intended. If such a correction is necessary, isolate it and explain it.

## Guardrails

- Keep the fork easy to sync from `JohnKinyanjui/sprite-maker`.
- Preserve working upstream architecture where practical.
- Prefer small, reviewable changes after testing.
- Do not spend time polishing release automation unless it blocks actual use.
- Do not turn the baseline into a redesign project.

## Done means

We can answer with evidence:

- Does current `main` build?
- Does the desktop app launch on a supported environment?
- Which major workflows actually work?
- What visibly breaks or causes friction?
- What one or two improvements are worth doing first?
