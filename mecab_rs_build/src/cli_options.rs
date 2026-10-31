use std::collections::HashMap;

const LONG_PREFIX: &str = "--";
const SHORT_PREFIX: char = '-';
const FLAG_ON: &str = "1";
const FLAG_OFF: &str = "0";

#[derive(Clone, Copy)]
pub struct OptionSpec {
    pub name: &'static str,
    pub short: Option<char>,
    pub takes_value: bool,
    pub default: Option<&'static str>,
}

#[derive(Clone, Copy)]
pub enum Arity {
    Flag,
    Text,
    Numbers(usize),
    Files,
}

#[derive(Clone, Copy)]
pub struct NamedSpec {
    pub name: &'static str,
    pub arity: Arity,
}

pub struct Occurrence<'a> {
    pub name: &'static str,
    pub value: &'a str,
}

#[derive(Debug, Default)]
pub struct Options {
    values: HashMap<String, Vec<String>>,
    occurrences: Vec<(&'static str, String)>,
    pub rest: Vec<String>,
}

struct Cursor<'a> {
    arguments: &'a [String],
    index: usize,
}

impl<'a> Cursor<'a> {
    fn new(arguments: &'a [String]) -> Self {
        Self {
            arguments,
            index: 0,
        }
    }

    fn next(&mut self) -> Option<&'a String> {
        let argument = self.arguments.get(self.index)?;
        self.index += 1;
        Some(argument)
    }

    fn next_if(&mut self, accept: impl Fn(&str) -> bool) -> Option<&'a String> {
        let argument = self.arguments.get(self.index)?;
        accept(argument).then(|| {
            self.index += 1;
            argument
        })
    }
}

fn unrecognized(argument: &str) -> String {
    format!("unrecognized option `{argument}`")
}

fn requires_argument(argument: &str) -> String {
    format!("`{argument}` requires an argument")
}

fn forbids_argument(argument: &str) -> String {
    format!("`{argument}` doesn't allow an argument")
}

fn split_long(long: &str) -> (&str, Option<&str>) {
    match long.split_once('=') {
        Some((key, inline)) => (key, Some(inline)),
        None => (long, None),
    }
}

pub fn lossy_arguments(arguments: &[std::ffi::OsString]) -> Vec<String> {
    arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect()
}

impl Options {
    pub fn parse(arguments: &[String], specs: &[OptionSpec]) -> Result<Self, String> {
        let mut options = Self::with_defaults(specs);
        let mut cursor = Cursor::new(arguments);
        while let Some(argument) = cursor.next() {
            if let Some(long) = argument.strip_prefix(LONG_PREFIX) {
                let (key, inline) = split_long(long);
                if key.is_empty() {
                    break;
                }
                options.parse_long(argument, (key, inline), specs, &mut cursor)?;
            } else if argument.len() > 1 && argument.starts_with(SHORT_PREFIX) {
                options.parse_short(argument, specs, &mut cursor)?;
            } else if !argument.starts_with(SHORT_PREFIX) {
                options.rest.push(argument.clone());
            }
        }
        Ok(options)
    }

    pub fn parse_named(
        command: &str,
        arguments: &[String],
        specs: &[NamedSpec],
    ) -> Result<Self, String> {
        let mut options = Self::default();
        let mut cursor = Cursor::new(arguments);
        while let Some(argument) = cursor.next() {
            if !argument.starts_with(LONG_PREFIX) {
                options.rest.push(argument.clone());
                continue;
            }
            let spec = specs
                .iter()
                .find(|spec| argument.strip_prefix(LONG_PREFIX) == Some(spec.name))
                .ok_or_else(|| format!("{command}: unknown option {argument}"))?;
            options.parse_named_values(command, argument, spec, &mut cursor)?;
        }
        Ok(options)
    }

    pub fn from_env(specs: &[OptionSpec]) -> Result<Self, String> {
        let arguments: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        Self::parse(&lossy_arguments(&arguments), specs)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.all(name).last().map(String::as_str)
    }

    pub fn all(&self, name: &str) -> &[String] {
        self.values.get(name).map_or(&[], Vec::as_slice)
    }

    pub fn occurrences(&self) -> impl Iterator<Item = Occurrence<'_>> {
        self.occurrences.iter().map(|(name, value)| Occurrence {
            name,
            value: value.as_str(),
        })
    }

    pub fn flag(&self, name: &str) -> bool {
        self.get(name).is_some_and(|value| value != FLAG_OFF)
    }

    pub fn number<T: std::str::FromStr + Default>(&self, name: &str) -> T {
        self.optional_number(name).unwrap_or_default()
    }

    pub fn numbers(&self, name: &str) -> Vec<usize> {
        self.all(name)
            .iter()
            .filter_map(|value| value.parse().ok())
            .collect()
    }

    pub fn optional_number<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.get(name).and_then(|value| value.trim().parse().ok())
    }

    fn with_defaults(specs: &[OptionSpec]) -> Self {
        let mut options = Self::default();
        for spec in specs {
            if let Some(default) = spec.default {
                options
                    .values
                    .insert(spec.name.to_owned(), vec![default.to_owned()]);
            }
        }
        options
    }

    fn record(&mut self, name: &'static str, value: String) {
        self.occurrences.push((name, value.clone()));
        self.values.insert(name.to_owned(), vec![value]);
    }

    fn parse_long(
        &mut self,
        argument: &str,
        (key, inline): (&str, Option<&str>),
        specs: &[OptionSpec],
        cursor: &mut Cursor,
    ) -> Result<(), String> {
        let spec = specs
            .iter()
            .find(|spec| spec.name == key)
            .ok_or_else(|| unrecognized(argument))?;
        let value = match (spec.takes_value, inline) {
            (true, Some(inline)) => inline.to_owned(),
            (true, None) => cursor
                .next()
                .cloned()
                .ok_or_else(|| requires_argument(argument))?,
            (false, Some(_)) => return Err(forbids_argument(argument)),
            (false, None) => FLAG_ON.to_owned(),
        };
        self.record(spec.name, value);
        Ok(())
    }

    fn parse_short(
        &mut self,
        argument: &str,
        specs: &[OptionSpec],
        cursor: &mut Cursor,
    ) -> Result<(), String> {
        let Some(short) = argument
            .strip_prefix(SHORT_PREFIX)
            .and_then(|rest| rest.chars().next())
        else {
            return Err(unrecognized(argument));
        };
        let spec = specs
            .iter()
            .find(|spec| spec.short == Some(short))
            .ok_or_else(|| unrecognized(argument))?;
        let attached = &argument[SHORT_PREFIX.len_utf8() + short.len_utf8()..];
        let value = match (spec.takes_value, attached.is_empty()) {
            (true, false) => attached.to_owned(),
            (true, true) => cursor
                .next()
                .cloned()
                .ok_or_else(|| requires_argument(argument))?,
            (false, true) => FLAG_ON.to_owned(),
            (false, false) => return Err(forbids_argument(argument)),
        };
        self.record(spec.name, value);
        Ok(())
    }

    fn parse_named_values(
        &mut self,
        command: &str,
        argument: &str,
        spec: &NamedSpec,
        cursor: &mut Cursor,
    ) -> Result<(), String> {
        let values = match spec.arity {
            Arity::Flag => vec![FLAG_ON.to_owned()],
            Arity::Text => vec![
                cursor
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{command}: {argument} needs a value"))?,
            ],
            Arity::Numbers(count) => (0..count)
                .map(|_| {
                    cursor
                        .next()
                        .filter(|value| value.parse::<usize>().is_ok())
                        .cloned()
                        .ok_or_else(|| format!("{command}: {argument} needs a number"))
                })
                .collect::<Result<_, _>>()?,
            Arity::Files => {
                let files = self.values.entry(spec.name.to_owned()).or_default();
                while let Some(file) = cursor.next_if(|next| !next.starts_with(LONG_PREFIX)) {
                    files.push(file.clone());
                }
                if files.is_empty() {
                    return Err(format!("{command}: {argument} needs a file"));
                }
                return Ok(());
            }
        };
        self.values.insert(spec.name.to_owned(), values);
        Ok(())
    }
}
