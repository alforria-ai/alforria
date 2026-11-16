//! Separate grep's patterns and option values from the files it opens.

use super::{command_word, parse, unquote};

enum Value {
    Pattern,
    PatternFile,
    Other,
}

fn record(value: Value, argument: String, explicit: &mut bool, files: &mut Vec<String>) {
    match value {
        Value::Pattern => *explicit = true,
        Value::PatternFile => {
            *explicit = true;
            files.push(argument);
        }
        Value::Other => {}
    }
}

/// Unknown options or shell expansions fall back to the general path scan.
pub(super) fn path_args(node: &parse::CommandNode) -> Option<Vec<String>> {
    let name = command_word(&node.parts.first()?.text);
    if !matches!(name.as_str(), "grep" | "egrep" | "fgrep") {
        return None;
    }
    let mut arguments = parse::literal_command(&node.source)?.into_iter();
    if command_word(&arguments.next()?) != name {
        return None;
    }
    let mut files = Vec::new();
    let mut operands = Vec::new();
    let mut explicit = false;
    let mut options = true;
    while let Some(argument) = arguments.next() {
        let text = unquote(&argument);
        if options && text == "--" {
            options = false;
        } else if options && text.starts_with("--") {
            let (name, inline) = text
                .split_once('=')
                .map_or((text, None), |(name, value)| (name, Some(value)));
            let kind = match name {
                "--regexp" => Value::Pattern,
                "--file" => Value::PatternFile,
                "--after-context" | "--before-context" | "--context" | "--max-count"
                | "--directories" | "--devices" | "--binary-files" | "--label" | "--include"
                | "--exclude" | "--exclude-dir" | "--include-dir" => Value::Other,
                "--color" | "--colour" => continue,
                "--extended-regexp"
                | "--fixed-strings"
                | "--basic-regexp"
                | "--perl-regexp"
                | "--ignore-case"
                | "--no-ignore-case"
                | "--word-regexp"
                | "--line-regexp"
                | "--line-buffered"
                | "--no-messages"
                | "--invert-match"
                | "--version"
                | "--help"
                | "--quiet"
                | "--silent"
                | "--binary"
                | "--text"
                | "--files-with-matches"
                | "--files-without-match"
                | "--count"
                | "--only-matching"
                | "--line-number"
                | "--byte-offset"
                | "--with-filename"
                | "--no-filename"
                | "--initial-tab"
                | "--null"
                | "--null-data"
                | "--recursive"
                | "--dereference-recursive"
                | "--mmap"
                    if inline.is_none() =>
                {
                    continue;
                }
                _ => return None,
            };
            let value = match inline {
                Some(value) => value.to_string(),
                None => arguments.next()?,
            };
            record(kind, value, &mut explicit, &mut files);
        } else if options && text.starts_with('-') && text != "-" {
            let short = &text[1..];
            // The traditional -NUM context shorthand.
            if short.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let mut flags = short.chars();
            while let Some(flag) = flags.next() {
                let kind = match flag {
                    'e' => Value::Pattern,
                    'f' => Value::PatternFile,
                    'A' | 'B' | 'C' | 'D' | 'd' | 'm' => Value::Other,
                    flag if "EFGPHIabchilLnoqRrsvVwxyZzUu".contains(flag) => continue,
                    _ => return None,
                };
                let value = if flags.as_str().is_empty() {
                    arguments.next()?
                } else {
                    flags.as_str().to_string()
                };
                record(kind, value, &mut explicit, &mut files);
                break;
            }
        } else {
            operands.push(argument);
        }
    }
    // -e/-f anywhere in the option list makes every positional operand a
    // file. Otherwise the first positional operand is the implicit pattern.
    files.extend(operands.into_iter().skip(usize::from(!explicit)));
    Some(files)
}
