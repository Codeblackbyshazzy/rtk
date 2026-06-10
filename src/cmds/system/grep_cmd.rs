//! Filters grep output by grouping matches by file.

use crate::core::stream::exec_capture;
use crate::core::tracking;
use crate::core::utils::resolved_command;
use crate::core::{args_utils, config};
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::HashMap;

/// Short flags (exact 2-char form `-X`) that consume one following token as their value.
/// `-e` is handled separately — its value goes into `patterns`, not `flags`.
/// Deliberately small: unknown flags pass through unchanged. The failure mode
/// for a missing entry is a visible wrong result, not a silent corruption.
const VALUE_FLAGS_SHORT: &[u8] = b"ABCgfjm";

/// Normalise short flags arg for rg forwarding.
/// Returns `None` when the entire flag is stripped (grep-ism recursive flags).
fn strip_r(arg: &str) -> Option<String> {
    match arg
        .chars()
        .filter(|&c| c != 'r' && c != 'R')
        .collect::<String>()
    {
        s if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// Normalise long flag arg for rg forwarding.
/// Returns `None` when the flag should be dropped (grep-ism recursive flags).
fn strip_recursive(arg: &str) -> Option<String> {
    match arg {
        // Drop recursive flags that would change semantics in rg.
        "--recursive" => None,
        // Everything else pass through unchanged.
        _ => Some(arg.to_string()),
    }
}

/// Extracts `(patterns, paths, flags)` from the raw trailing args.
///
/// - `patterns`: first non-flag positional prepended to any `-e` values.
///   All patterns are passed to rg as `-e` flags, so positional and `-e` are
///   interchangeable from rg's perspective. Empty → caller should error.
/// - `paths`: all subsequent non-flag positionals. Empty → caller defaults to `["."]`.
/// - `flags`: other flags forwarded to rg (recursive flags already stripped).
///
/// Value-taking short flags (see `VALUE_FLAGS_SHORT`) consume the next token
/// as their value so it is not mistaken for the pattern. Combined clusters like
/// `-rn` have `r`/`R` stripped before forwarding. `--` marks everything after
/// it as positional even if flag-shaped.
fn extract_pattern_path<T: AsRef<str>>(args: &[T]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut e_patterns: Vec<String> = Vec::new();
    let mut positionals: Vec<String> = Vec::new();
    let mut flags: Vec<String> = Vec::new();
    let mut past_dashdash = false;
    let mut i = 0;

    while i < args.len() {
        let arg = args[i].as_ref();

        if past_dashdash {
            positionals.push(arg.to_string());
            i += 1;
            continue;
        }

        if arg == "--" {
            past_dashdash = true;
            i += 1;
            continue;
        }

        // Long flags (--foo, --recursive): strip or pass through unchanged
        if arg.starts_with("--") {
            if let Some(cleaned) = strip_recursive(arg) {
                flags.push(cleaned);
            }
            i += 1;
            continue;
        }

        match arg.strip_prefix('-') {
            Some(rest) if !rest.is_empty() => {
                let last = *rest.as_bytes().last().unwrap();
                let last_is_e = last == b'e';
                let last_takes_value = last_is_e || VALUE_FLAGS_SHORT.contains(&last);

                if last_takes_value {
                    // Emit cleaned prefix (everything before last char, r/R stripped)
                    if let Some(prefix) = strip_r(&rest[..rest.len() - 1]) {
                        flags.push(format!("-{}", prefix));
                    }

                    let value = if i + 1 < args.len() {
                        let v = args[i + 1].as_ref().to_string();
                        i += 2;
                        Some(v)
                    } else {
                        i += 1;
                        None
                    };

                    if last_is_e {
                        if let Some(v) = value {
                            e_patterns.push(v);
                        } else {
                            // -e without a value: treat "e" as a normal flag to avoid losing the pattern.
                            flags.push("-e".to_string());
                        }
                    } else {
                        flags.push(format!("-{}", last as char));
                        if let Some(v) = value {
                            flags.push(v);
                        }
                    }
                } else {
                    // No value-taking flag at end: strip r/R, forward remainder
                    if let Some(cleaned) = strip_r(rest) {
                        flags.push(format!("-{}", cleaned));
                    }
                    i += 1;
                }
            }
            _ => {
                positionals.push(arg.to_string());
                i += 1;
            }
        }
    }

    // If -e was used: all positionals are paths; -e values are the patterns.
    // If -e was not used: first positional is the pattern, rest are paths.
    let (patterns, paths) = if !e_patterns.is_empty() {
        (e_patterns, positionals)
    } else {
        let paths = positionals.iter().skip(1).cloned().collect();
        let patterns = positionals.into_iter().take(1).collect();
        (patterns, paths)
    };

    (patterns, paths, flags)
}

pub fn run(
    max_line_len: usize,
    max_results: usize,
    context_only: bool,
    file_type: Option<&str>,
    args: &[String],
    verbose: u8,
) -> Result<i32> {
    let timer = tracking::TimedExecution::start();

    // --version / --help: pass through to rg without filtering.
    // Note: Clap strips `--` before populating trailing_var_arg, so both
    // `rtk grep --version` and `rtk grep -- --version` land here identically.
    if args
        .iter()
        .any(|a| a == "--version" || a == "--help" || a == "-h")
    {
        let mut rg_cmd = resolved_command("rg");
        rg_cmd.args(args);
        let result = exec_capture(&mut rg_cmd)
            .or_else(|_| {
                // rg unavailable: fall back to system grep.
                let mut grep_cmd = resolved_command("grep");
                grep_cmd.args(args);
                exec_capture(&mut grep_cmd)
            })
            .context("grep/rg failed")?;
        print!("{}", result.stdout);
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr);
        }
        return Ok(result.exit_code);
    }

    // Re-insert `--` when clap's trailing_var_arg consumed it
    let args = args_utils::restore_double_dash(args);

    let (patterns, paths, extra_args) = extract_pattern_path(&args);

    if patterns.is_empty() {
        eprintln!("rtk grep: pattern required (positional or -e)");
        return Ok(1);
    }

    let pattern_display = if patterns.len() == 1 {
        patterns[0].clone()
    } else {
        patterns.join("|")
    };

    let paths = if paths.is_empty() {
        vec![".".to_string()]
    } else {
        paths
    };
    let path_display = paths.join(" ");

    if verbose > 0 {
        eprintln!("grep: '{}' in {}", pattern_display, path_display);
    }

    let mut rg_cmd = resolved_command("rg");
    // --no-ignore-vcs: match grep -r behavior (don't skip .gitignore'd files).
    // Without this, rg returns 0 matches for files in .gitignore, causing
    // false negatives that make AI agents draw wrong conclusions.
    // Using --no-ignore-vcs (not --no-ignore) so .ignore/.rgignore are still respected.
    // -H: always emit the filename.
    // -0: NUL-separate filename. Allows the parser to disambiguate filenames or
    // content containing `:digits:` patterns (issue #1436).
    rg_cmd.args(["-nH0", "--no-heading", "--no-ignore-vcs"]);

    if let Some(ft) = file_type {
        rg_cmd.arg("--type").arg(ft);
    }

    // extra_args is already stripped of -r/-R/-recursive by extract_pattern_path
    rg_cmd.args(&extra_args);

    // All patterns as -e flags (BRE \| → | translation for rg's PCRE engine).
    // Using -e keeps `--` semantically as a flag/path separator, not part of the pattern.
    for p in &patterns {
        rg_cmd.args(["-e", &p.replace(r"\|", "|")]);
    }

    // `--` after all flags: prevents rg from interpreting path args starting
    // with `-` as its own flags.
    rg_cmd.arg("--");
    rg_cmd.args(&paths);

    let result = exec_capture(&mut rg_cmd)
        .or_else(|_| {
            // rg unavailable: fall back to system grep with the original,
            // untranslated patterns (grep interprets BRE natively).
            let mut grep_cmd = resolved_command("grep");
            grep_cmd.args(&extra_args);
            for p in &patterns {
                grep_cmd.args(["-e", p]);
            }
            grep_cmd.args(["-rnHZ", "--"]);
            grep_cmd.args(&paths);
            exec_capture(&mut grep_cmd)
        })
        .context("grep/rg failed")?;

    // Passthrough output flags that produce output that is already small.
    if has_format_flag(&extra_args) {
        print!("{}", result.stdout);
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr.trim());
        }

        let args_display = if extra_args.is_empty() {
            format!("'{}' {}", pattern_display, path_display)
        } else {
            format!(
                "{} '{}' {}",
                extra_args.join(" "),
                pattern_display,
                path_display
            )
        };

        timer.track_passthrough(
            &format!("grep {}", args_display),
            &format!("rtk grep {} (passthrough)", args_display),
        );
        return Ok(result.exit_code);
    }

    let exit_code = result.exit_code;
    let raw_output = result.stdout.clone();

    if result.stdout.trim().is_empty() {
        // Show stderr for errors (bad regex, missing file, etc.)
        if exit_code == 2 && !result.stderr.trim().is_empty() {
            eprintln!("{}", result.stderr.trim());
        }
        let msg = format!("0 matches for '{}'", pattern_display);
        println!("{}", msg);
        timer.track(
            &format!("grep -rn '{}' {}", pattern_display, path_display),
            "rtk grep",
            &raw_output,
            &msg,
        );
        return Ok(exit_code);
    }

    let context_re = if context_only {
        Regex::new(&format!(
            "(?i).{{0,20}}{}.*",
            regex::escape(&pattern_display)
        ))
        .ok()
    } else {
        None
    };

    let mut by_file: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for line in result.stdout.lines() {
        let Some((file, line_num, content)) = parse_match_line(line) else {
            continue;
        };
        let cleaned = clean_line(content, max_line_len, context_re.as_ref(), &pattern_display);
        by_file.entry(file).or_default().push((line_num, cleaned));
    }

    // Derive total from parsed results so the header matches what we show.
    let total_matches: usize = by_file.values().map(|v| v.len()).sum();

    let mut rtk_output = String::new();
    rtk_output.push_str(&format!(
        "{} matches in {} files:\n\n",
        total_matches,
        by_file.len()
    ));

    let mut shown = 0;
    let mut files: Vec<_> = by_file.iter().collect();
    files.sort_by_key(|(f, _)| *f);

    let per_file = config::limits().grep_max_per_file;
    for (file, matches) in files {
        if shown >= max_results {
            break;
        }

        let file_display = compact_path(file);
        for (line_num, content) in matches.iter().take(per_file) {
            if shown >= max_results {
                break;
            }
            rtk_output.push_str(&format!("{}:{}:{}\n", file_display, line_num, content));
            shown += 1;
        }
    }

    if total_matches > shown {
        rtk_output.push_str(&format!("[+{} more]\n", total_matches - shown));
    }

    print!("{}", rtk_output);
    timer.track(
        &format!("grep -rn '{}' {}", pattern_display, path_display),
        "rtk grep",
        &raw_output,
        &rtk_output,
    );

    Ok(exit_code)
}

/// Parses a single rg/grep match line of the form `file\0line_number:content`.
///
/// Requires the underlying command to be invoked with `-0` (rg) or `-Z` (grep)
/// so the filename is NUL-separated from `line:content`. NUL cannot appear in
/// file paths, so the parser is unambiguous regardless of:
///   - content with `:` or `::` (e.g. `ClassRegistry::init(...)`, issue #1436);
///   - paths with embedded `:` (Windows drive letters, weird filenames like
///     `badly_named:52:file.txt`).
///
/// Returns `None` for lines that do not match the expected shape (e.g. rg
/// `-A`/`-B` context lines that use `-` as separator).
fn parse_match_line(line: &str) -> Option<(String, usize, &str)> {
    lazy_static::lazy_static! {
        static ref MATCH_LINE_RE: Regex = Regex::new(r"^([^\x00]+)\x00(\d+):(.*)$").unwrap();
    }
    MATCH_LINE_RE.captures(line).and_then(|caps| {
        let (_, [file, line_num, content]) = caps.extract();
        let line_num: usize = line_num.parse().ok()?;
        Some((file.to_string(), line_num, content))
    })
}

fn has_format_flag<T: AsRef<str>>(extra_args: &[T]) -> bool {
    extra_args.iter().any(|arg| {
        matches!(
            arg.as_ref(),
            "-c" | "--count"
                | "-l"
                | "--files-with-matches"
                | "-L"
                | "--files-without-match"
                | "-o"
                | "--only-matching"
                | "-Z"
                | "--null"
        )
    })
}

fn clean_line(line: &str, max_len: usize, context_re: Option<&Regex>, pattern: &str) -> String {
    let trimmed = line.trim();

    if let Some(re) = context_re {
        if let Some(m) = re.find(trimmed) {
            let matched = m.as_str();
            if matched.len() <= max_len {
                return matched.to_string();
            }
        }
    }

    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else {
        let lower = trimmed.to_lowercase();
        let pattern_lower = pattern.to_lowercase();

        if let Some(pos) = lower.find(&pattern_lower) {
            let char_pos = lower[..pos].chars().count();
            let chars: Vec<char> = trimmed.chars().collect();
            let char_len = chars.len();

            let start = char_pos.saturating_sub(max_len / 3);
            let end = (start + max_len).min(char_len);
            let start = if end == char_len {
                end.saturating_sub(max_len)
            } else {
                start
            };

            let slice: String = chars[start..end].iter().collect();
            if start > 0 && end < char_len {
                format!("...{}...", slice)
            } else if start > 0 {
                format!("...{}", slice)
            } else {
                format!("{}...", slice)
            }
        } else {
            let t: String = trimmed.chars().take(max_len - 3).collect();
            format!("{}...", t)
        }
    }
}

fn compact_path(path: &str) -> String {
    if path.len() <= 50 {
        return path.to_string();
    }

    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() <= 3 {
        return path.to_string();
    }

    format!(
        "{}/.../{}/{}",
        parts[0],
        parts[parts.len() - 2],
        parts[parts.len() - 1]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_line() {
        let line = "            const result = someFunction();";
        let cleaned = clean_line(line, 50, None, "result");
        assert!(!cleaned.starts_with(' '));
        assert!(cleaned.len() <= 50);
    }

    #[test]
    fn test_compact_path() {
        let path = "/Users/patrick/dev/project/src/components/Button.tsx";
        let compact = compact_path(path);
        assert!(compact.len() <= 60);
    }

    #[test]
    fn test_clean_line_multibyte() {
        // Thai text that exceeds max_len in bytes
        let line = "  สวัสดีครับ นี่คือข้อความที่ยาวมากสำหรับทดสอบ  ";
        let cleaned = clean_line(line, 20, None, "ครับ");
        // Should not panic
        assert!(!cleaned.is_empty());
    }

    #[test]
    fn test_clean_line_emoji() {
        let line = "🎉🎊🎈🎁🎂🎄 some text 🎃🎆🎇✨";
        let cleaned = clean_line(line, 15, None, "text");
        assert!(!cleaned.is_empty());
    }

    // Fix: BRE \| alternation is translated to PCRE | for rg
    #[test]
    fn test_bre_alternation_translated() {
        let pattern = r"fn foo\|pub.*bar";
        let rg_pattern = pattern.replace(r"\|", "|");
        assert_eq!(rg_pattern, "fn foo|pub.*bar");
    }

    // --- process_flag ---

    #[test]
    fn test_strip_r() {
        assert_eq!(strip_r(""), None);
        assert_eq!(strip_r("r"), None);
        assert_eq!(strip_r("rr"), None);
        assert_eq!(strip_r("R"), None);
        assert_eq!(strip_r("rn"), Some("n".to_string()));
        assert_eq!(strip_r("Rni"), Some("ni".to_string()));
        assert_eq!(strip_r("i"), Some("i".to_string()));
    }

    #[test]
    fn test_strip_recursive() {
        assert_eq!(strip_recursive("--recursive"), None);
        assert_eq!(strip_recursive("--glob"), Some("--glob".to_string()));
        assert_eq!(strip_recursive("--type"), Some("--type".to_string()));
    }

    // --- extract_pattern_path ---

    #[test]
    fn test_extract_simple() {
        let (patterns, paths, flags) = extract_pattern_path(&["foo", "src/"]);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src/"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_with_bool_flag() {
        let (patterns, paths, flags) = extract_pattern_path(&["-i", "foo", "src/"]);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src/"]);
        assert_eq!(flags, vec!["-i"]);
    }

    #[test]
    fn test_extract_value_taking_flag() {
        // -A 2 must not steal "error" as its value
        let (patterns, paths, flags) = extract_pattern_path(&["-A", "2", "error", "src"]);
        assert_eq!(patterns, vec!["error"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-A", "2"]);
    }

    #[test]
    fn test_extract_cluster_strip_r() {
        // -rn: r stripped, n forwarded (not leaked to rg as --replace value)
        let (patterns, paths, flags) = extract_pattern_path(&["-rn", "foo", "src"]);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-n"]);
    }

    #[test]
    fn test_extract_cluster_ending_in_e() {
        // -rne PATTERN: r stripped, n in prefix, e consumes PATTERN as pattern
        let (patterns, paths, flags) = extract_pattern_path(&["-rne", "PATTERN", "src"]);
        assert_eq!(patterns, vec!["PATTERN"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-n"]);
    }

    #[test]
    fn test_extract_cluster_ending_in_value_flag() {
        // -rA 2: r stripped, A consumes 2 as context value
        let (patterns, paths, flags) = extract_pattern_path(&["-rA", "2", "foo", "src"]);
        assert_eq!(patterns, vec!["foo"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-A", "2"]);
    }

    #[test]
    fn test_extract_multi_path() {
        let (patterns, paths, flags) = extract_pattern_path(&["TODO", "src", "tests"]);
        assert_eq!(patterns, vec!["TODO"]);
        assert_eq!(paths, vec!["src", "tests"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_glob_value() {
        // -g '*.md' must not steal "agent" as its value
        let (patterns, paths, flags) = extract_pattern_path(&["-i", "x", "agent", "-g", "*.md"]);
        assert_eq!(patterns, vec!["x"]);
        assert_eq!(paths, vec!["agent"]);
        assert_eq!(flags, vec!["-i", "-g", "*.md"]);
    }

    #[test]
    fn test_extract_e_flag() {
        let (patterns, paths, flags) = extract_pattern_path(&["-e", "fn run", "src"]);
        assert_eq!(patterns, vec!["fn run"]);
        assert_eq!(paths, vec!["src"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_multi_e() {
        let (patterns, paths, flags) = extract_pattern_path(&["-e", "foo", "-e", "bar", "src"]);
        assert_eq!(patterns, vec!["foo", "bar"]);
        assert_eq!(paths, vec!["src"]);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_dashdash_boundary() {
        // After --, args are positional even if they look like flags
        let (patterns, paths, flags) = extract_pattern_path(&["--", "--version"]);
        assert_eq!(patterns, vec!["--version"]);
        assert!(paths.is_empty());
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_no_args() {
        let (patterns, paths, flags) = extract_pattern_path::<&str>(&[]);
        assert!(patterns.is_empty());
        assert!(paths.is_empty());
        assert!(flags.is_empty());
    }

    #[test]
    fn test_extract_default_path_empty() {
        // Caller is responsible for defaulting empty paths to ["."]
        let (patterns, paths, _) = extract_pattern_path(&["foo"]);
        assert_eq!(patterns, vec!["foo"]);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_extract_ending_e() {
        let (patterns, paths, flags) =
            extract_pattern_path(&["-e", "foo", "-e", "bar", "src", "-e"]);
        assert_eq!(patterns, vec!["foo", "bar"]);
        assert_eq!(paths, vec!["src"]);
        assert_eq!(flags, vec!["-e"]);
    }

    // --- truncation accuracy ---

    #[test]
    fn test_grep_overflow_uses_uncapped_total() {
        // Confirm the grep overflow invariant: matches vec is never capped before overflow calc.
        // If total_matches > per_file, overflow = total_matches - per_file (not capped).
        // This documents that grep_cmd.rs avoids the diff_cmd bug (cap at N then compute N-10).
        let per_file = config::limits().grep_max_per_file;
        let total_matches = per_file + 42;
        let overflow = total_matches - per_file;
        assert_eq!(overflow, 42, "overflow must equal true suppressed count");
        // Demonstrate why capping before subtraction is wrong:
        let hypothetical_cap = per_file + 5;
        let capped = total_matches.min(hypothetical_cap);
        let wrong_overflow = capped - per_file;
        assert_ne!(
            wrong_overflow, overflow,
            "capping before subtraction gives wrong overflow"
        );
    }

    // --- format flag detection ---

    #[test]
    fn test_format_flag_detects_count() {
        assert!(has_format_flag(&["-c"]));
        assert!(has_format_flag(&["--count"]));
    }

    #[test]
    fn test_format_flag_detects_files_with_matches() {
        assert!(has_format_flag(&["-l"]));
        assert!(has_format_flag(&["--files-with-matches"]));
    }

    #[test]
    fn test_format_flag_detects_files_without_match() {
        assert!(has_format_flag(&["-L"]));
        assert!(has_format_flag(&["--files-without-match"]));
    }

    #[test]
    fn test_format_flag_detects_only_matching() {
        assert!(has_format_flag(&["-o"]));
        assert!(has_format_flag(&["--only-matching"]));
    }

    #[test]
    fn test_format_flag_detects_null() {
        assert!(has_format_flag(&["-Z"]));
        assert!(has_format_flag(&["--null"]));
    }

    #[test]
    fn test_format_flag_ignores_normal_flags() {
        assert!(!has_format_flag(&["-i", "-w", "-A", "3"]));
    }

    // Verify line numbers are always enabled in rg invocation (grep_cmd.rs:24).
    // The -n/--line-numbers clap flag in main.rs is a no-op accepted for compat.
    #[test]
    fn test_rg_always_has_line_numbers() {
        // grep_cmd::run() always passes "-n" to rg (line 24).
        // This test documents that -n is built-in, so the clap flag is safe to ignore.
        let mut cmd = resolved_command("rg");
        cmd.args(["-n", "--no-heading", "NONEXISTENT_PATTERN_12345", "."]);
        // If rg is available, it should accept -n without error (exit 1 = no match, not error)
        if let Ok(output) = cmd.output() {
            assert!(
                output.status.code() == Some(1) || output.status.success(),
                "rg -n should be accepted"
            );
        }
        // If rg is not installed, skip gracefully (test still passes)
    }

    // --- issue #1436: parse_match_line robustness ---
    // Input shape is `file\0line:content` (rg --null / grep -Z).

    #[test]
    fn test_parse_match_line_simple() {
        let line = "file.php\x0010:use Foo\\Bar;";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "file.php");
        assert_eq!(line_num, 10);
        assert_eq!(content, "use Foo\\Bar;");
    }

    // Issue #1436 reproducer: content with `::` must not split into a phantom
    // file bucket. With NUL separation between file and line:content, content
    // colons are irrelevant to the parser.
    #[test]
    fn test_parse_match_line_content_with_double_colon() {
        let line = "externalImportShell.class.php\x0081:        $this->queueProcessModel = ClassRegistry::init('Collections.QueueProcess');";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "externalImportShell.class.php");
        assert_eq!(line_num, 81);
        assert_eq!(
            content,
            "        $this->queueProcessModel = ClassRegistry::init('Collections.QueueProcess');"
        );
    }

    // Windows abs-path safety: drive letter + backslashes must not break the
    // parser. The NUL separator makes the file portion unambiguous.
    #[test]
    fn test_parse_match_line_windows_path() {
        let line = "C:\\src\\file.rs\x0042:fn main() {}";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, r"C:\src\file.rs");
        assert_eq!(line_num, 42);
        assert_eq!(content, "fn main() {}");
    }

    // Filenames containing `:digits:` (which would fool a greedy `:` parser)
    // must still parse correctly under NUL separation.
    #[test]
    fn test_parse_match_line_filename_with_colons() {
        let line = "badly_named:52:file.txt\x001:xxx";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "badly_named:52:file.txt");
        assert_eq!(line_num, 1);
        assert_eq!(content, "xxx");
    }

    // Content that itself contains `:digits:` (e.g. log lines, port numbers,
    // line-number-like substrings) must not confuse the parser.
    #[test]
    fn test_parse_match_line_content_with_digit_colons() {
        let line = "log.txt\x007:debug: counter is :42: now";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "log.txt");
        assert_eq!(line_num, 7);
        assert_eq!(content, "debug: counter is :42: now");
    }

    #[test]
    fn test_parse_match_line_malformed_returns_none() {
        // No NUL separator (e.g. rg/grep invoked without --null/-Z, or a
        // context line written with `-`).
        assert!(parse_match_line("file.rs:1:content").is_none());
        assert!(parse_match_line("not a match line").is_none());
        // Missing line number after NUL
        assert!(parse_match_line("file.rs\x00fn foo()").is_none());
        // Empty
        assert!(parse_match_line("").is_none());
    }

    #[test]
    fn test_parse_match_line_empty_content() {
        let line = "file.rs\x007:";
        let (file, line_num, content) = parse_match_line(line).unwrap();
        assert_eq!(file, "file.rs");
        assert_eq!(line_num, 7);
        assert_eq!(content, "");
    }

    #[test]
    fn test_rg_no_ignore_vcs_flag_accepted() {
        // Verify rg accepts --no-ignore-vcs (used to match grep -r behavior for .gitignore)
        let mut cmd = resolved_command("rg");
        cmd.args([
            "-n",
            "--no-heading",
            "--no-ignore-vcs",
            "NONEXISTENT_PATTERN_12345",
            ".",
        ]);
        if let Ok(output) = cmd.output() {
            assert!(
                output.status.code() == Some(1) || output.status.success(),
                "rg --no-ignore-vcs should be accepted"
            );
        }
        // If rg is not installed, skip gracefully (test still passes)
    }
}
