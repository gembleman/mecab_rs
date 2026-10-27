use std::collections::HashMap;

#[derive(Clone, Copy)]
pub struct OptionSpec {
    pub name: &'static str,
    pub short: Option<char>,
    pub takes_value: bool,
    pub default: Option<&'static str>,
}

#[derive(Debug, Default)]
pub struct Options {
    values: HashMap<String, String>,
    pub rest: Vec<String>,
}

impl Options {
    pub fn parse(arguments: &[String], specs: &[OptionSpec]) -> Result<Self, String> {
        let mut options = Self::default();
        for spec in specs {
            if let Some(default) = spec.default {
                options
                    .values
                    .insert(spec.name.to_owned(), default.to_owned());
            }
        }
        let mut index = 0;
        while index < arguments.len() {
            let argument = &arguments[index];
            index += 1;
            if let Some(long) = argument.strip_prefix("--") {
                let (key, inline) = match long.split_once('=') {
                    Some((key, inline)) => (key, Some(inline)),
                    None => (long, None),
                };
                if key.is_empty() {
                    break;
                }
                let spec = specs
                    .iter()
                    .find(|spec| spec.name == key)
                    .ok_or_else(|| format!("unrecognized option `{argument}`"))?;
                if spec.takes_value {
                    let value = match inline {
                        Some(inline) => inline.to_owned(),
                        None => {
                            index += 1;
                            arguments
                                .get(index - 1)
                                .cloned()
                                .ok_or_else(|| format!("`{argument}` requires an argument"))?
                        }
                    };
                    options.values.insert(spec.name.to_owned(), value);
                } else if inline.is_some() {
                    return Err(format!("`{argument}` doesn't allow an argument"));
                } else {
                    options.values.insert(spec.name.to_owned(), "1".to_owned());
                }
            } else if argument.starts_with('-') && argument.len() > 1 {
                let short = argument.chars().nth(1).expect("longer than one character");
                let spec = specs
                    .iter()
                    .find(|spec| spec.short == Some(short))
                    .ok_or_else(|| format!("unrecognized option `{argument}`"))?;
                let attached = &argument[1 + short.len_utf8()..];
                if spec.takes_value {
                    let value = if attached.is_empty() {
                        index += 1;
                        arguments
                            .get(index - 1)
                            .cloned()
                            .ok_or_else(|| format!("`{argument}` requires an argument"))?
                    } else {
                        attached.to_owned()
                    };
                    options.values.insert(spec.name.to_owned(), value);
                } else if attached.is_empty() {
                    options.values.insert(spec.name.to_owned(), "1".to_owned());
                } else {
                    return Err(format!("`{argument}` doesn't allow an argument"));
                }
            } else if !argument.starts_with('-') {
                options.rest.push(argument.clone());
            }
        }
        Ok(options)
    }

    pub fn from_env(specs: &[OptionSpec]) -> Result<Self, String> {
        let arguments: Vec<String> = std::env::args_os()
            .skip(1)
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        Self::parse(&arguments, specs)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    pub fn flag(&self, name: &str) -> bool {
        self.get(name).is_some_and(|value| value != "0")
    }

    pub fn number<T: std::str::FromStr + Default>(&self, name: &str) -> T {
        self.get(name)
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or_default()
    }
}
