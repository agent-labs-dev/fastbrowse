//! The part of Python's `argparse` the three commands use, so a command line means and reports what it did there.
//!
//! Long options with unique-prefix abbreviation and `--flag=value`, one positional, `-h`, repeatable options and a
//! mutually exclusive pair. Errors are argparse's words, after the usage, with exit status 2. The help text is
//! argparse's own output at 80 columns, kept beside the source, since reflowing it here would be a second formatter
//! to keep in step.

use std::collections::HashMap;

pub use fastbrowse_core::py::lax::py_float;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `store_true`.
    Flag,
    /// One value; the last one given wins.
    Value,
    /// `action="append"`.
    Append,
    /// `action="version"`.
    Version,
    /// `-h, --help`, which every parser has.
    Help,
}

#[derive(Debug, Clone, Copy)]
pub struct Opt {
    pub flag: &'static str,
    pub kind: Kind,
    pub required: bool,
}

impl Opt {
    pub const fn flag(flag: &'static str) -> Opt {
        Opt { flag, kind: Kind::Flag, required: false }
    }
    pub const fn value(flag: &'static str) -> Opt {
        Opt { flag, kind: Kind::Value, required: false }
    }
    pub const fn append(flag: &'static str) -> Opt {
        Opt { flag, kind: Kind::Append, required: false }
    }
    pub const fn version() -> Opt {
        Opt { flag: "--version", kind: Kind::Version, required: false }
    }
    pub const fn required(self) -> Opt {
        Opt { required: true, ..self }
    }
}

pub struct Parser {
    pub prog: &'static str,
    /// `format_help()`; the usage is its first paragraph.
    pub help: &'static str,
    pub version: Option<String>,
    pub options: Vec<Opt>,
    pub positional: Option<&'static str>,
    pub exclusive: Vec<(&'static str, &'static str)>,
}

/// What a command line asked for, or what to print and exit with instead.
#[derive(Debug, Default)]
pub struct Parsed {
    values: HashMap<&'static str, Vec<String>>,
    pub positional: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    /// Text for stdout, then exit 0: `--help`, `--version`.
    Print(String),
    /// Text for stderr, then exit 2.
    Usage(String),
}

impl Parsed {
    pub fn flag(&self, flag: &str) -> bool {
        self.values.contains_key(flag)
    }

    pub fn value(&self, flag: &str) -> Option<&str> {
        self.values.get(flag).and_then(|values| values.last()).map(String::as_str)
    }

    pub fn all(&self, flag: &str) -> &[String] {
        self.values.get(flag).map_or(&[], Vec::as_slice)
    }
}

/// argparse's test for an argument that is a negative number, and so a value rather than an option.
fn negative_number(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else { return false };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(rest)
        || rest
            .split_once('.')
            .is_some_and(|(whole, fraction)| whole.bytes().all(|b| b.is_ascii_digit()) && digits(fraction))
}

impl Parser {
    pub fn usage(&self) -> &str {
        self.help.split("\n\n").next().unwrap_or(self.help)
    }

    /// `parser.error(message)`: the usage and the message, for stderr.
    pub fn error(&self, message: &str) -> Exit {
        Exit::Usage(format!("{}\n{}: error: {message}\n", self.usage(), self.prog))
    }

    /// `parser.error` for an option's value, as a `type=` callable's `ArgumentTypeError` reads.
    pub fn bad_value(&self, flag: &str, message: &str) -> Exit {
        self.error(&format!("argument {flag}: {message}"))
    }

    fn find(&self, given: &str) -> Result<Option<Opt>, Exit> {
        // argparse adds `-h, --help` ahead of every other option, which is the order an ambiguity lists them in.
        let help = Opt { flag: "--help", kind: Kind::Help, required: false };
        let options = || std::iter::once(&help).chain(self.options.iter());
        if given == "-h" {
            return Ok(Some(help));
        }
        if let Some(exact) = options().find(|opt| opt.flag == given) {
            return Ok(Some(*exact));
        }
        let matches: Vec<&Opt> =
            options().filter(|opt| given.starts_with("--") && opt.flag.starts_with(given)).collect();
        match matches.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(**one)),
            many => {
                let names: Vec<&str> = many.iter().map(|opt| opt.flag).collect();
                Err(self.error(&format!("ambiguous option: {given} could match {}", names.join(", "))))
            }
        }
    }

    /// Read `args`, checking each value with `check` as argparse applies `type=` while it reads: the first bad value
    /// on the line is the one reported.
    pub fn parse(
        &self,
        args: &[String],
        check: impl Fn(&'static str, &str) -> Result<(), String>,
    ) -> Result<Parsed, Exit> {
        let mut parsed = Parsed::default();
        let mut extras: Vec<String> = Vec::new();
        let mut seen_exclusive: Option<&'static str> = None;
        let mut only_positionals = false;
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            index += 1;
            if only_positionals || !arg.starts_with('-') || arg == "-" || negative_number(arg) {
                if parsed.positional.is_none() && self.positional.is_some() {
                    parsed.positional = Some(arg.clone());
                } else {
                    extras.push(arg.clone());
                }
                continue;
            }
            if arg == "--" {
                only_positionals = true;
                continue;
            }
            let (name, inline) = match arg.split_once('=') {
                Some((name, value)) if name.starts_with("--") => (name, Some(value.to_string())),
                _ => (arg.as_str(), None),
            };
            let Some(opt) = self.find(name)? else {
                extras.push(arg.clone());
                continue;
            };
            for (a, b) in &self.exclusive {
                if opt.flag == *a || opt.flag == *b {
                    match seen_exclusive {
                        Some(other) if other != opt.flag => {
                            return Err(
                                self.error(&format!("argument {}: not allowed with argument {other}", opt.flag))
                            );
                        }
                        _ => seen_exclusive = Some(opt.flag),
                    }
                }
            }
            match opt.kind {
                Kind::Flag | Kind::Version | Kind::Help => {
                    if let Some(value) = inline {
                        let value = fastbrowse_core::py::text::repr(&value);
                        let flag = if opt.kind == Kind::Help { "-h/--help" } else { opt.flag };
                        return Err(self.error(&format!("argument {flag}: ignored explicit argument {value}")));
                    }
                    if opt.kind == Kind::Help {
                        return Err(Exit::Print(self.help.to_string()));
                    }
                    if opt.kind == Kind::Version {
                        return Err(Exit::Print(format!("{}\n", self.version.clone().unwrap_or_default())));
                    }
                    parsed.values.entry(opt.flag).or_default();
                }
                Kind::Value | Kind::Append => {
                    let value = match inline {
                        Some(value) => value,
                        None => match args.get(index) {
                            Some(next) if !next.starts_with('-') || negative_number(next) || next == "-" => {
                                index += 1;
                                next.clone()
                            }
                            _ => return Err(self.error(&format!("argument {}: expected one argument", opt.flag))),
                        },
                    };
                    check(opt.flag, &value).map_err(|message| self.bad_value(opt.flag, &message))?;
                    let values = parsed.values.entry(opt.flag).or_default();
                    if opt.kind == Kind::Value {
                        values.clear();
                    }
                    values.push(value);
                }
            }
        }
        let mut missing: Vec<&str> = self
            .options
            .iter()
            .filter(|opt| opt.required && !parsed.values.contains_key(opt.flag))
            .map(|opt| opt.flag)
            .collect();
        if let (Some(name), None) = (self.positional, &parsed.positional) {
            missing.push(name);
        }
        if !missing.is_empty() {
            return Err(self.error(&format!("the following arguments are required: {}", missing.join(", "))));
        }
        if !extras.is_empty() {
            return Err(self.error(&format!("unrecognized arguments: {}", extras.join(" "))));
        }
        Ok(parsed)
    }
}

/// `int(text)`, which takes surrounding whitespace, a sign and `_` between digits.
pub fn py_int(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let well_formed = !digits.is_empty()
        && !digits.starts_with('_')
        && !digits.ends_with('_')
        && !digits.contains("__")
        && digits.chars().all(|c| c.is_ascii_digit() || c == '_');
    if !well_formed {
        return None;
    }
    trimmed.replace('_', "").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> Parser {
        Parser {
            prog: "fastbrowse",
            help: "usage: fastbrowse task\n\nRun one browser task.\n",
            version: Some("fastbrowse 1".into()),
            options: vec![
                Opt::value("--max-steps"),
                Opt::value("--max-dollars"),
                Opt::flag("--json"),
                Opt::flag("--cloud"),
                Opt::flag("--local"),
                Opt::append("--secret"),
                Opt::version(),
            ],
            positional: Some("task"),
            exclusive: vec![("--cloud", "--local")],
        }
    }

    fn run(args: &[&str]) -> Result<Parsed, Exit> {
        parser().parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>(), |_, _| Ok(()))
    }

    fn usage(message: &str) -> Exit {
        Exit::Usage(format!("usage: fastbrowse task\nfastbrowse: error: {message}\n"))
    }

    #[test]
    fn options_abbreviate_to_a_unique_prefix() {
        let parsed = run(&["t", "--max-s=3", "--js", "--secret", "a=B", "--secret", "c=D"]).unwrap();
        assert_eq!(parsed.value("--max-steps"), Some("3"));
        assert!(parsed.flag("--json"));
        assert_eq!(parsed.all("--secret"), ["a=B", "c=D"]);
        assert_eq!(
            run(&["t", "--max"]).unwrap_err(),
            usage("ambiguous option: --max could match --max-steps, --max-dollars")
        );
    }

    #[test]
    fn argparse_reports_what_it_reports() {
        assert_eq!(run(&[]).unwrap_err(), usage("the following arguments are required: task"));
        assert_eq!(run(&["a", "b"]).unwrap_err(), usage("unrecognized arguments: b"));
        assert_eq!(
            run(&["t", "--cloud", "--local"]).unwrap_err(),
            usage("argument --local: not allowed with argument --cloud")
        );
        assert_eq!(run(&["t", "--max-steps"]).unwrap_err(), usage("argument --max-steps: expected one argument"));
        assert_eq!(run(&["t", "--json="]).unwrap_err(), usage("argument --json: ignored explicit argument ''"));
        assert_eq!(run(&["--max-steps", "-5", "t"]).unwrap().value("--max-steps"), Some("-5"));
        assert_eq!(run(&["--version"]).unwrap_err(), Exit::Print("fastbrowse 1\n".into()));
        assert!(matches!(run(&["t", "--he"]).unwrap_err(), Exit::Print(_)));
        assert_eq!(run(&["--", "--json"]).unwrap().positional.as_deref(), Some("--json"));
    }

    #[test]
    fn numbers_read_as_python_reads_them() {
        assert_eq!(py_int(" +1_000 "), Some(1000));
        assert_eq!(py_int("1.5"), None);
        assert_eq!(py_float("1e3"), Some(1000.0));
        assert_eq!(py_float("x"), None);
        assert!(py_float("-inf").is_some_and(f64::is_infinite));
    }
}
