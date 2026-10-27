# @reef-tui/cli

Reef — a terminal UI for Git and code review.

## Install

```sh
npm install -g @reef-tui/cli
# or run directly
npx @reef-tui/cli
```

## Usage

```sh
reef
# or
rf
```

See the [project README](https://github.com/Blushyes/reef#readme) for full documentation.

## Update

For a global npm installation:

```sh
reef update
```

The launcher checks that it belongs to the active npm global installation, then runs
`npm install --global @reef-tui/cli@latest`. Use the Node/npm environment that installed Reef.
For a project-local installation, run `npm install @reef-tui/cli@latest` in the project.
For a temporary run, use `npx @reef-tui/cli@latest`.

## Supported platforms

- macOS (arm64, x64)
- Linux (arm64, x64, glibc)
- Windows (x64)

The correct native binary is installed automatically via `optionalDependencies`.
