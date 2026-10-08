//! Just enough of the GraphQL grammar to name the operation type of every
//! definition in a document, so `atb linear query` can refuse writes before
//! anything is sent.

#[derive(Debug, PartialEq, Eq)]
pub enum Definition {
    Query,
    Mutation,
    Subscription,
    Fragment,
}

/// The kind of each top-level definition, in order. Strings and comments are
/// skipped and brackets are counted, so a word inside an argument, a string
/// or a selection set is never mistaken for an operation keyword.
pub fn definitions(document: &str) -> Result<Vec<Definition>, String> {
    let bytes = document.as_bytes();
    let mut found = Vec::new();
    let mut depth = 0usize;
    let mut i = 0;
    // True at the start of the document and after each top-level selection
    // set closes: the next name or `{` begins a definition.
    let mut expect_definition = true;
    while i < bytes.len() {
        let byte = bytes[i];
        match byte {
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                i += 3;
                loop {
                    if i >= bytes.len() {
                        return Err("unterminated block string".into());
                    }
                    if bytes[i..].starts_with(b"\\\"\"\"") {
                        i += 4;
                    } else if bytes[i..].starts_with(b"\"\"\"") {
                        i += 3;
                        break;
                    } else {
                        i += 1;
                    }
                }
                continue;
            }
            b'"' => {
                i += 1;
                loop {
                    match bytes.get(i) {
                        None | Some(b'\n') => return Err("unterminated string".into()),
                        Some(b'\\') => i += 2,
                        Some(b'"') => {
                            i += 1;
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
                continue;
            }
            b'{' | b'(' | b'[' => {
                if depth == 0 && expect_definition {
                    if byte != b'{' {
                        return Err(format!("unexpected `{}`", byte as char));
                    }
                    found.push(Definition::Query);
                    expect_definition = false;
                }
                depth += 1;
            }
            b'}' | b')' | b']' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("unbalanced `{}`", byte as char))?;
                if depth == 0 && byte == b'}' {
                    expect_definition = true;
                }
            }
            b'_' | b'a'..=b'z' | b'A'..=b'Z' => {
                let start = i;
                while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                if depth == 0 && expect_definition {
                    found.push(match &document[start..i] {
                        "query" => Definition::Query,
                        "mutation" => Definition::Mutation,
                        "subscription" => Definition::Subscription,
                        "fragment" => Definition::Fragment,
                        other => return Err(format!("unexpected `{other}` at the top level")),
                    });
                    expect_definition = false;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return Err("unbalanced brackets".into());
    }
    if !found.iter().any(|d| *d != Definition::Fragment) {
        return Err("no operation in the document".into());
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::Definition::*;
    use super::*;

    #[test]
    fn names_each_top_level_definition() {
        assert_eq!(definitions("{ viewer { id } }").unwrap(), vec![Query]);
        assert_eq!(
            definitions("query Q($a: [Int] = [1]) @x(y: {z: 1}) { a(b: \"}\") }\nmutation M { m }")
                .unwrap(),
            vec![Query, Mutation]
        );
        assert_eq!(
            definitions("fragment F on Issue { id }\nsubscription S { s }").unwrap(),
            vec![Fragment, Subscription]
        );
    }

    #[test]
    fn a_keyword_inside_an_argument_string_or_comment_is_not_an_operation() {
        let document = r#"# mutation
            query { issues(filter: {title: {contains: "mutation { x }"}}) { nodes { mutation: id } }
              d: issue(id: """ \""" mutation """) { id } }"#;
        assert_eq!(definitions(document).unwrap(), vec![Query]);
    }

    #[test]
    fn rejects_documents_it_cannot_read() {
        assert!(definitions("").is_err());
        assert!(definitions("{ a ").is_err());
        assert!(definitions("viewer { id }").is_err());
        assert!(definitions("fragment F on T { id }").is_err());
    }
}
