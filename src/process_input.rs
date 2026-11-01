//! Literal process arguments and bounded standard input. Never repair a command
//! by silently interpreting it as a shell program or widening its permission.
use serde_json::Value;

pub(crate) const STDIN_LIMIT: usize = 32_768;

pub(crate) fn invocation_issue(arguments: &Value) -> Option<&'static str> {
    let argv = arguments["argv"].as_array()?;
    let program = argv.first()?.as_str()?.rsplit(['/', '\\']).next()?;
    let interpreter = matches!(
        program,
        "python" | "python3" | "node" | "nodejs" | "ruby" | "perl" | "php" | "lua"
    ) || program
        .strip_prefix("python3.")
        .is_some_and(|version| !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()));
    // A stdin option with an attached shell here-document is not a valid
    // interpreter option. Do not reject arbitrary literal strings or valid
    // shell scripts passed via -c; their meaning belongs to that program.
    if interpreter
        && argv.get(1).and_then(Value::as_str).is_some_and(|arg| {
            arg.strip_prefix('-').is_some_and(|rest| {
                rest.starts_with(char::is_whitespace) && rest.trim_start().starts_with("<<")
            })
        })
    {
        return Some(
            "argv is passed literally, without shell parsing. A here-document attached to '-' is not an interpreter option. For Python use argv:[\"python3\",\"-\"] with stdin containing only the script, or argv:[\"python3\",\"-c\",script]. For shell syntax use an explicit shell and -c. Correct the invocation before changing the application; no application check ran.",
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_interpreter_input_is_not_silently_executed_as_shell() {
        for program in ["python3", "/usr/bin/python3.12", "node", "ruby", "perl"] {
            assert!(
                invocation_issue(&json!({"argv":[program,"- <<'END'\nprint(1)\nEND"]})).is_some()
            );
        }
        for argv in [
            json!(["python3", "-c", "print('<<END')"]),
            json!(["python3", "-"]),
            json!(["sh", "-c", "python3 - <<'END'\nprint(1)\nEND"]),
            json!(["printf", "%s", "- <<'END'\ntext\nEND"]),
            json!(["custom-reader", "- <<data"]),
        ] {
            assert!(invocation_issue(&json!({"argv":argv})).is_none());
        }
    }
}
