# 📝 Comprehensive Feature Integration Plan: `nh sys switch` 🚀

## 🌟 Overview

The `nh sys switch` command will allow users of the `nh` tool to seamlessly execute and compute system environments managed by `system-manager`. This new feature simplifies interacting with Nix-managed system environments and exporting their computed variables.

## 🎯 New Feature Specification

### 🎛️ Command

* **Syntax**: `nh sys switch [<flake>] [--export] [--json] [--write <file>]`

### 📋 Flags

* `--export`: Outputs the environment variables in shell-compatible `export` format.
* `--json`: Outputs the environment in JSON format.
* `--write <file>`: Saves the environment variables to a specified file.

## 🔧 Implementation Details

### 📦 New Module: `src/sys.rs`

#### ⚙️ Functional Breakdown:

* **CLI parsing** (using Clap)

  * Defines and parses the `sys` subcommand.
  * Handles flags: `--export`, `--json`, `--write`.

* **System-manager Integration**

  * Uses existing `Command` abstraction (`commands.rs`) to invoke system-manager.
  * Captures output (JSON or shell-compatible environment variables).

* **Output Handling**

  * Formats the captured data:

    * Shell (`export VAR=value`)
    * JSON (`{ "VAR": "value" }`)
    * File write (`VAR=value` per line)

#### 📐 Est. Lines of Code: **\~300**

### 🖥️ CLI Updates (`src/interface.rs`)

* Add the new `sys switch` subcommand and associated options (`--export`, `--json`, `--write`).

#### 📐 Est. Lines of Code: **\~30**

## 🗃️ Documentation Updates

### 📚 README.md

* 🎉 Introduce and showcase the `sys switch` command.
* Provide practical examples demonstrating each flag:

```sh
nh sys switch . --export
nh sys switch .#my-host --json
nh sys switch . --write .env
```

### 📖 Man Page (via `xtask/src/man.rs`)

* Add `nh sys switch` examples to auto-generated man pages.

### 📜 Changelog (`CHANGELOG.md`)

* Document the new command and its capabilities:

```
🚀 **New:** Added `nh sys switch` for easy system-manager environment exports.
```

### 📂 Shell Completions

* Regenerate shell completions (`bash`, `zsh`, `fish`) using existing `xtask` functionality.

## 🧪 Testing Strategy

* ✅ Unit tests in Rust:

  * Test command invocation and parsing logic.
  * Test environment output formatting (JSON, shell, file output).
* ✅ CLI integration tests:

  * Verify end-to-end functionality and subprocess handling.

## ⚠️ Impacted Areas & Considerations

### 🔄 Existing Modules

* `commands.rs`: Utilize existing subprocess handling.
* `interface.rs`: Extend CLI definition.

### 📦 New Modules

* `sys.rs`: Encapsulates new logic for system-manager interactions.

### 🛠️ Dependencies

* Assumes availability of `system-manager`.
* Uses existing crates: `serde_json`, `subprocess`, `clap`, `owo-colors`.

## 🚩 Open Questions

* Should the command default to outputting in a specific format if no flags are provided?
* Do we want to support automatic detection or implicit handling of flakes?

## 📝 Example Usage Scenario

### 📌 Basic usage:

```sh
nh sys switch .
```

### 🌍 Export environment variables to shell:

```sh
eval $(nh sys switch . --export)
```

### 📄 Write environment to file:

```sh
nh sys switch .#my-host --write .env
```

### 📦 Output environment in JSON:

```sh
nh sys switch . --json
```

## ✅ Final Checklist

* [ ] Implement `sys.rs` module.
* [ ] Update CLI definitions in `interface.rs`.
* [ ] Regenerate and update documentation (`README.md`, man pages).
* [ ] Add new entries in `CHANGELOG.md`.
* [ ] Write comprehensive tests.
* [ ] Update shell completions.

## 🗺️ Roadmap & Phased Goals

### 📅 Phase 1: Planning & CLI Design

* [ ] Finalize CLI interface and flags
* [ ] Determine expected output formats and default behavior
* [ ] Outline UX flow for `--export`, `--json`, and `--write`

### 🔨 Phase 2: Core Implementation

* [ ] Implement `sys.rs` logic to invoke `system-manager` and capture output
* [ ] Parse output into a structured form (e.g., key-value pairs)
* [ ] Implement output formatting logic (shell, JSON, file)

### 🔌 Phase 3: Integration

* [ ] Add new subcommand to Clap CLI in `interface.rs`
* [ ] Register help text, usage, and error handling
* [ ] Wire up CLI options to core logic in `sys.rs`

### 🧪 Phase 4: Testing & Validation

* [ ] Add unit tests for output parsing and formatting
* [ ] Add CLI integration tests for all output formats
* [ ] Manual testing with real `system-manager` usage

### 📚 Phase 5: Documentation & Distribution

* [ ] Update `README.md` with feature overview and examples
* [ ] Update `xtask/src/man.rs` and regenerate man pages
* [ ] Regenerate shell completions
* [ ] Add changelog entry

### 🎉 Phase 6: Release

* [ ] Merge feature branch
* [ ] Announce feature in next release
* [ ] Monitor for bug reports and feedback

