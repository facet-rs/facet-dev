# facet-dev

[![Coverage Status](https://coveralls.io/repos/github/facet-rs/facet-dev/badge.svg?branch=main)](https://coveralls.io/github/facet-rs/facet?branch=main)
[![crates.io](https://img.shields.io/crates/v/facet-dev.svg)](https://crates.io/crates/facet-dev)
[![documentation](https://docs.rs/facet-dev/badge.svg)](https://docs.rs/facet-dev)
[![MIT/Apache-2.0 licensed](https://img.shields.io/crates/l/facet-dev.svg)](./LICENSE)
[![Discord](https://img.shields.io/discord/1379550208551026748?logo=discord&label=discord)](https://discord.gg/JhD7CwCJ8F)

**facet-dev** is a comprehensive development automation tool for Rust workspaces.
It integrates seamlessly as a pre-commit hook and handles repetitive project setup,
code generation, and CI/CD configuration automatically.

## Features

facet-dev automates the following tasks:

- **README Generation**: Generates `README.md` files from `README.md.in` templates with customizable headers and footers
- **CI/CD Pipeline Setup**: Creates GitHub Actions workflows for testing, coverage, and quality checks
- **Funding Configuration**: Sets up GitHub funding information (`FUNDING.yml`)
- **Pre-commit Hooks**: Installs and manages git hooks via [cargo-husky](https://lib.rs/crates/cargo-husky)
- **Code Formatting**: Runs `cargo fmt` to enforce consistent code style
- **Pre-push Verification**: Validates code before pushing (via `facet-dev pre-push`):
  - Runs clippy for linting
  - Executes tests
  - Checks documentation compilation
  - Verifies all crates in workspace
- **MSRV Consistency**: Ensures all crates have the same Minimum Supported Rust Version
- **Rust Documentation**: Auto-generates rustdoc configuration

## Installation

Install facet-dev from crates.io:

```bash
cargo install facet-dev
```

Or build from source:

```bash
cargo install --git https://github.com/facet-rs/facet-dev
```

## Usage

### Basic Generation

Run facet-dev in your workspace root to generate/update all project files:

```bash
facet-dev
```

This will:
1. Generate `README.md` for the workspace and all crates
2. Set up GitHub Actions workflows
3. Configure funding information
4. Install and configure pre-commit hooks
5. Format code with `cargo fmt`
6. Stage all changes with `git add`

### Pre-push Checks

Before pushing to a remote repository, run:

```bash
facet-dev pre-push
```

This performs comprehensive checks:
- Runs `cargo clippy` on all crates
- Executes `cargo test`
- Validates documentation with `cargo doc`
- Ensures MSRV consistency

### Custom Template Directory

Specify a custom directory for looking up `README.md.in` templates:

```bash
facet-dev --template-dir /path/to/templates
```

This searches for `{crate_name}.md.in` files in the specified directory, falling back
to the crate's own template if not found.

### Debugging

View workspace metadata and package information:

```bash
facet-dev debug-packages
```

## README Template Configuration

### Standard Template Format

Each crate should have a `README.md.in` file in its directory. The generated `README.md`
combines three parts:

1. **Header** (with badges and links)
2. **Main Content** (from `README.md.in`)
3. **Footer** (with sponsors and license information)

### Custom Header and Footer

To customize the README header and footer templates project-wide, create a `.facet-dev-templates`
directory at your workspace root with custom templates:

```bash
.facet-dev-templates/
├── readme-header.md
└── readme-footer.md
```

**Header Template Example** (`readme-header.md`):

```markdown
# {CRATE}

[![CI](https://github.com/your-org/{CRATE}/actions/workflows/ci.yml/badge.svg)](https://github.com/your-org/{CRATE}/actions)
[![crates.io](https://img.shields.io/crates/v/{CRATE}.svg)](https://crates.io/crates/{CRATE})
```

The `{CRATE}` placeholder will be replaced with the actual crate name.

**Footer Template Example** (`readme-footer.md`):

```markdown
## License

Licensed under the MIT License.
```

If these files don't exist, facet-dev uses the built-in default templates.

### Template Priority

For `README.md.in` templates (main content):

1. Custom directory specified via `--template-dir` (if provided)
2. Crate's own `README.md.in` file (in the crate directory)
3. Workspace-level `README.md.in` (for the workspace README)

## Pre-commit Hook Setup

facet-dev includes a helper script to install git hooks for the repository and all
worktrees. This is typically done automatically during initial setup, but you can
run it manually:

```bash
./scripts/setup.sh
```

This installs hooks that:
- **pre-commit**: Runs `facet-dev` to auto-generate and stage files
- **pre-push**: Runs `facet-dev pre-push` for comprehensive validation

## Notes

### Automatic Staging

When facet-dev runs, it automatically stages all generated files with `git add`.
If facet-dev is updated mid-development, the new changes might appear in an unrelated PR.
This is by design—keep facet-dev stable during active work.

### Workspace Requirements

Your project should:
- Be a Rust workspace or single crate with `Cargo.toml`
- Have git initialized
- Have `README.md.in` template files for any crates you want documented

## Sponsors

Thanks to all individual sponsors:

<p> <a href="https://github.com/sponsors/fasterthanlime">
<picture>
<source media="(prefers-color-scheme: dark)" srcset="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/github-dark.svg">
<img src="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/github-light.svg" height="40" alt="GitHub Sponsors">
</picture>
</a> <a href="https://patreon.com/fasterthanlime">
    <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/patreon-dark.svg">
    <img src="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/patreon-light.svg" height="40" alt="Patreon">
    </picture>
</a> </p>

...along with corporate sponsors:

<p> <a href="https://aws.amazon.com">
<picture>
<source media="(prefers-color-scheme: dark)" srcset="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/aws-dark.svg">
<img src="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/aws-light.svg" height="40" alt="AWS">
</picture>
</a> <a href="https://zed.dev">
<picture>
<source media="(prefers-color-scheme: dark)" srcset="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/zed-dark.svg">
<img src="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/zed-light.svg" height="40" alt="Zed">
</picture>
</a> <a href="https://depot.dev?utm_source=facet">
<picture>
<source media="(prefers-color-scheme: dark)" srcset="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/depot-dark.svg">
<img src="https://github.com/facet-rs/facet/raw/main/static/sponsors-v3/depot-light.svg" height="40" alt="Depot">
</picture>
</a> </p>

...without whom this work could not exist.

## Special thanks

The facet logo was drawn by [Misiasart](https://misiasart.com/).

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/facet-rs/facet/blob/main/LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](https://github.com/facet-rs/facet/blob/main/LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
