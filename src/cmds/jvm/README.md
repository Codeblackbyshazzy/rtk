# JVM ecosystem filters

Filters for JVM-based build tools.

| Module           | Tool(s)                              | Modes                                                                                  |
|------------------|--------------------------------------|----------------------------------------------------------------------------------------|
| `gradlew_cmd.rs` | `./gradlew`, `gradlew.bat`, `gradle` | Build / Test / ConnectedTest / Lint / Dependencies — streaming line filter + passthrough |
| `mvn_cmd.rs`     | `mvn`, `./mvnw`, `mvnw.cmd`          | Test / Compile / Package / Passthrough — buffered single-pass filter per phase           |

## Maven (`mvn_cmd.rs`)

Phase routing (`detect_phase`):

| Phase       | Goals                                                  | Filter                  |
|-------------|--------------------------------------------------------|-------------------------|
| `Test`      | `test`, `integration-test` (Failsafe = Surefire shape) | `filter_surefire`       |
| `Compile`   | `compile`, `test-compile`                              | `filter_compile`        |
| `Package`   | `package`, `install`, `verify`, `deploy`               | `filter_package`        |
| `Passthrough` | `clean`, `site`, `dependency:*`, `--version`, `--help`, empty, any unrecognised goal | none |

Key behaviours:

- **ANSI strip first** in every filter — real Maven output contains colour escapes.
- **English-footer guard** — if neither `BUILD SUCCESS` nor `BUILD FAILURE` appears as a trimmed line suffix, return the ANSI-stripped raw input unchanged. Protects non-English locales.
- **Verbose bypass** — `-X`, `--debug`, `-e`, `--errors` skip filtering (`run_passthrough`). User asked for detail; respect it.
- **Surefire block collapse** — Surefire emits `[INFO] Running <FQN>` … `[INFO] Tests run: N, Failures: F, Errors: E, …, Time elapsed: T s - in <FQN>`. The filter buffers each block and emits it only when `F > 0` or `E > 0`. Passing blocks (the bulk of healthy-project output) are dropped silently. Failing blocks are emitted with framework stack frames stripped via a deny-list (`at org.junit.`, `at java.util.`, `at sun.reflect.`, etc.).
- **Multi-failure classes (trail re-arm)** — when a single class has several failing tests, Surefire 3.x emits one blank-separated detail block per failing test under a single close line. When a failure trail ends at a blank line, the state machine arms a re-entry: the next per-test subline (`[ERROR] FQN.method -- Time elapsed: … <<< FAILURE!` or `<<< ERROR!`) re-enters the trail with the same keep/drop decision, so every failure message survives (and a capped class drops *all* its blocks). Any other non-blank line disarms the re-entry.
- **`<<< ERROR!` markers** — per-test sublines use `<<< ERROR!` for thrown (non-assertion) exceptions; the close-line regex also tolerates an `ERROR!` marker defensively (Surefire 3.5.5 emits `FAILURE!` even for errors-only classes — failure detection keys off the `Failures`/`Errors` counts, not the marker).
- **Help-boilerplate stripping (all modes)** — the post-failure block Maven emits after `[ERROR] Failed to execute goal` (`See …`, `-> [Help 1]`, `Re-run Maven`, `To see the full stack trace`, `For more information`, help URLs, bare `[ERROR]` dividers) is dropped in quiet *and* non-quiet filters alike (shared `BOILER_PREFIXES`). Deliberately kept as signal: `Failed to execute goal` itself and the multi-module resume hint (`[ERROR] After correcting the problems…` + `[ERROR]   mvn <args> -rf :module` — tells the user/agent how to resume the build). Real durations (`Time elapsed: … s`, `Total time: …`) ship untouched — the numbers are diagnostic signal.
- **Wrapper detection** — `./mvnw` (POSIX) and `mvnw.cmd` (Windows) detected via string-literal `Command::new` (semgrep-safe); falls back to `resolved_command("mvn")`.
- **Reactor Summary preservation** — for multi-module builds, the trailing `Reactor Summary for <root>` block with per-module SUCCESS/FAILURE rows is kept (toggled by a `[INFO] Reactor Summary for ` header and cleared on `BUILD SUCCESS` / `BUILD FAILURE`).
- **Failure cap** — both the count of emitted failing test classes and the size of the `[ERROR] Failures:` summary block are bounded by `limits.mvn_max_failures` (default `25`). Excess emissions are replaced by a single `... +N more failing test classes` / `... +N more failures` tail to keep large failure sets compact. Set the limit to `0` in `config.toml` to opt out of the cap entirely.

Token-savings tests run inline as part of `cargo test --all` and verify ≥90% savings for `mvn test` and ≥85% for `mvn install` on full synthetic fixtures (gzipped, ~1100 lines each). The `flate2` dependency (already in `Cargo.toml`) decompresses the ~3 KB gzipped fixtures in milliseconds.

### Integrity-check whitelist

`Commands::Mvn` is intentionally omitted from `is_operational_command` in `src/main.rs`, matching the gradle precedent (`Commands::Gradlew` also omitted). The whitelist guards SHA-256 hook-integrity verification; filter modules invoked through an already-verified hook do not need a second check on their own dispatch path. Per the comment above the function, the whitelist is opt-in by design and a forgotten command fails open rather than creating false confidence about what's protected.

## Gradle (`gradlew_cmd.rs`)

See module docs and the gradle PR (`feat/gradlew-android-support`) for rationale. Streaming filter chosen because Gradle output is task-line-based, not block-based — unlike Maven Surefire.
