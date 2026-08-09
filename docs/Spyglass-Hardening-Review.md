# Spyglass Sprite Studio hardening review

Status: pre-installation review
Branch: `review/hardening-v1`
Upstream baseline: `ac8ca4668fcdcb208354dc27132182df4f8b111f`

## Goal

Preserve Sprite Studio's useful local-first asset workflow while making the fork safe enough to install on a development workstation and practical for a Godot-based 2D game pipeline.

## What is strong upstream

- Local project files remain ordinary assets instead of being trapped in a hosted service.
- SQLite stores project metadata while PNG and export files remain inspectable on disk.
- Character animation uses one locked master and deterministic pixel-region transforms instead of independently regenerating every frame.
- Asset versions use content hashes and repairs are designed to be non-destructive.
- Provider arguments are passed directly to a process instead of through a shell.
- Generated sprite categories and rig source paths have meaningful path/category validation.
- Sprite-sheet dimensions and animation frame counts are bounded.

## Release-blocking hardening

### 1. Recursive workspace deletion

Upstream allowed a registered workspace directory to be recursively deleted after only a shallow path-depth check.

Fork policy:

- Every registered workspace receives a `.sprite-studio/workspace.json` ownership marker.
- The marker records the workspace id and exact canonical path.
- A folder opened as an existing directory is non-destructive by default.
- Recursive deletion is permitted only for a directory created as a new Sprite Studio workspace and only when the marker matches the registered id and canonical path.
- Removing a workspace registration remains separate from deleting its files.

Implemented on this branch.

### 2. Desktop webview policy

Upstream disabled Content Security Policy with `csp: null`.

Fork policy:

- Enable a restrictive local CSP.
- Permit only Tauri IPC and the local asset protocol required to display project images.
- Do not add remote script or CDN origins.

Implemented on this branch.

### 3. Package identity

The upstream bundle identifier must not be reused by this fork.

Fork identifier: `com.spyglassofodysseus.sprite-studio`.

Implemented on this branch.

### 4. Automated validation

No local installation is approved until the following gates pass from a clean checkout:

- locked Bun dependency install
- Svelte/TypeScript check
- Rust formatting check
- Rust test suite
- Clippy with warnings denied
- clean Windows NSIS installer build

A GitHub Actions workflow implementing these gates is included on this branch.

### 5. AI generation isolation

Current upstream behavior launches Codex in `workspace-write` with the full project root as the working directory. The prompt asks the agent to preserve unrelated files, but the filesystem boundary still allows project writes.

Required design before this is considered fully hardened:

1. Create a per-generation staging workspace.
2. Copy only the render tools and required source/reference assets into staging.
3. Run the provider with staging as its writable working directory.
4. Validate the generation manifest, output paths, dimensions, alpha, file counts, and hashes.
5. Promote only validated outputs into the real project using Sprite Studio's versioning layer.
6. Preserve failed staging jobs for diagnosis without modifying production assets.

Do not simulate this protection with prompt wording alone.

## Production improvements after hardening

### Character identity contract

Add a persistent character identity containing:

- canonical master asset
- palette
- logical canvas
- baseline/ground line
- reusable rig and anatomical pivots
- named equipment/accessory parts
- approved animation set

Animations should reference the identity rather than rediscovering the character on every request.

### Corrective pixel layer

Keep deterministic rigging as the base animation system, then permit a small non-destructive per-frame corrective pixel layer for joints, hands, feet, perspective changes, cloth, hair, and equipment.

### Godot export

Add a first-class export target for Godot that emits:

- sprite sheet PNG
- stable animation metadata
- frame durations
- pivots/offsets
- named animation clips
- optional generated `SpriteFrames` resource

The exporter must be deterministic and versioned.

### Palette locking

For pixel-art projects, validate frames against an explicit project/character palette rather than only measuring broad palette similarity.

### Game-specific playground

Allow project-specific playground harnesses so movement speed, jump arcs, ledges, vines, ladders, hazards, enemies, pivots, and collision bounds can be tested with the real animation timing before export.

## Installation gate

Do not install the fork on the primary workstation until:

1. CI quality gates pass.
2. A Windows NSIS artifact is produced from the reviewed commit.
3. The installer artifact can be traced to that commit.
4. A temporary test workspace passes create/open/remove/delete safety checks.
5. One imported master asset completes an animation/export round trip without modifying unrelated assets.

Only after these checks should the reviewed branch be merged or tagged for installation.
