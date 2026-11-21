# nier_coop  

Experimental two-player co-op mod for NieR:Automata (Steam). A DLL injected into the game spawns a second 2B
driven by the other player's inputs over UDP.

>  **Made entirely with Claude.** All of the code, reverse engineering and documentation in this repository was
> written by [Claude](https://claude.com/claude-code) (Anthropic's AI, via Claude Code), directed and tested
> in-game by a human.

## Status

Works in a local mirror test: movement, attacks, pod fire (incl. free aim) and pod skills sync; the second 2B
is invincible and respawns after area loads. Flight-unit sections are not supported. See [CLAUDE.md](CLAUDE.md)
for details.

## Build

Requirements:

- [Rust](https://rustup.rs) (the nightly toolchain pinned in `rust-toolchain.toml` is installed automatically).

- The Microsoft C++ build tools, which provide the MSVC linker (`link.exe`). VS Code is not enough. Install

Visual Studio 2022 Build Tools with the C++ workload:

  
```
winget install --id Microsoft.VisualStudio.2022.BuildTools -e --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"

```

  

Or run the [Build Tools installer](https://visualstudio.microsoft.com/visual-cpp-build-tools/) and pick

"Desktop development with C++".

  

Then build:

  

```
cargo build --release --target x86_64-pc-windows-msvc
```

To make the zip for players, run `package\build.ps1`, which writes `dist\nier_coop.zip`. Each player unzips it
and runs `Install.bat`, which copies the DLL into the game folder as `dinput8.dll` (the game loads it at startup, no
injector) and writes `nier_coop.cfg`. Steps are in `package\README.txt`.

Manual install: copy `nier_coop.cfg.example` to `nier_coop.cfg` next to `NieRAutomata.exe`, set `bind` and `peer`,
then put `nier_coop.dll` there as `dinput8.dll`, or inject it into the running game.

## Contributing

Issues and pull requests are welcome. The open problem is flight-unit sections; the rules for reversed
structs and pointer safety are in [CLAUDE.md](CLAUDE.md). Run `cargo test` before sending a PR (the ignored
tests need `NIER_EXE=<path to NieRAutomata.exe>`).

## License

[GPL-3.0](LICENSE): you may use, change and share it, but changed versions you distribute must stay GPL-3.0
with their source available.

## Credits

Offsets, patterns and the partner-spawn method are based on
[praydog/AutomataMP](https://github.com/praydog/AutomataMP) (MIT).
