use std::borrow::Cow;

pub mod instances;

pub trait Configuration {
    type Content<'a>;

    fn default_closing_whitespace() -> &'static [char];
    fn infer_url<'a>(_content: &Self::Content<'a>) -> Option<Cow<'a, str>> {
        None
    }

    /// Return closing whitespace for the given line, if it is not the default.
    #[must_use]
    fn non_default_closing_whitespace(line: &str) -> Option<Vec<char>> {
        let mut is_match = true;
        let mut chars_read = 0;
        let mut reversed_chars = line.chars().rev();

        for whitespace_char in Self::default_closing_whitespace() {
            if let Some(next_char) = reversed_chars.next() {
                if *whitespace_char == next_char {
                    chars_read += 1;
                } else {
                    if crate::closing_whitespace::is_json_whitespace(next_char) {
                        chars_read += 1;
                    }

                    is_match = false;
                    break;
                }
            } else {
                is_match = false;
                break;
            }
        }

        for next_char in reversed_chars {
            if crate::closing_whitespace::is_json_whitespace(next_char) {
                chars_read += 1;
            } else {
                break;
            }
        }

        if is_match && chars_read == Self::default_closing_whitespace().len() {
            None
        } else {
            let mut closing_whitespace = line.chars().rev().take(chars_read).collect::<Vec<_>>();
            closing_whitespace.reverse();

            Some(closing_whitespace)
        }
    }
}

impl Configuration for () {
    type Content<'a> = serde_json::Value;

    fn default_closing_whitespace() -> &'static [char] {
        &[]
    }
}
