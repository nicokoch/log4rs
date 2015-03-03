#![feature(std_misc, fs, io, path, core, old_io)]
#![warn(missing_doc)]

extern crate log;
extern crate time;
extern crate "toml" as toml_parser;

use std::borrow::ToOwned;
use std::cmp;
use std::collections::HashMap;
use std::error;
use std::fs;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf, AsPath};
use std::sync::{Mutex, Arc};
use std::old_io::timer::sleep;
use std::thread;
use std::time::Duration;
use log::{LogLevel, LogRecord, LogLevelFilter, SetLoggerError};

use toml::Creator;

pub mod toml;
pub mod config;
pub mod appender;
pub mod pattern;

/// A trait implemented by log4rs appenders.
pub trait Append: Send + 'static{
    /// Processes the provided `LogRecord`.
    fn append(&mut self, record: &LogRecord) -> Result<(), Box<error::Error>>;
}

struct ConfiguredLogger {
    level: LogLevelFilter,
    appenders: Vec<usize>,
    children: Vec<(String, Box<ConfiguredLogger>)>,
}

impl ConfiguredLogger {
    fn add(&mut self, path: &str, mut appenders: Vec<usize>, additive: bool, level: LogLevelFilter) {
        let (part, rest) = match path.find("::") {
            Some(idx) => (&path[..idx], &path[idx+2..]),
            None => (path, ""),
        };

        for &mut (ref child_part, ref mut child) in &mut self.children {
            if &child_part[..] == part {
                child.add(rest, appenders, additive, level);
                return;
            }
        }

        let child = if rest.is_empty() {
            if additive {
                appenders.extend(self.appenders.iter().cloned());
            }

            ConfiguredLogger {
                level: level,
                appenders: appenders,
                children: vec![],
            }
        } else {
            let mut child = ConfiguredLogger {
                level: self.level,
                appenders: self.appenders.clone(),
                children: vec![],
            };
            child.add(rest, appenders, additive, level);
            child
        };

        self.children.push((part.to_owned(), Box::new(child)));
    }

    fn max_log_level(&self) -> LogLevelFilter {
        let mut max = self.level;
        for &(_, ref child) in &self.children {
            max = cmp::max(max, child.max_log_level());
        }
        max
    }

    fn find(&self, path: &str) -> &ConfiguredLogger {
        let mut node = self;

        'parts: for part in path.split("::") {
            for &(ref child_part, ref child) in &node.children {
                if &child_part[..] == part {
                    node = child;
                    continue 'parts;
                }
            }

            break;
        }

        node
    }

    fn enabled(&self, level: LogLevel) -> bool {
        self.level >= level
    }

    fn log(&self, record: &log::LogRecord, appenders: &mut [Box<Append>]) {
        if self.enabled(record.level()) {
            for &idx in &self.appenders {
                if let Err(err) = appenders[idx].append(record) {
                    handle_error(&*err);
                }
            }
        }
    }
}

struct SharedLogger {
    root: ConfiguredLogger,
    appenders: Vec<Box<Append>>,
}

impl SharedLogger {
    fn new(config: config::Config) -> SharedLogger {
        let config::Config { appenders, root, loggers, .. } = config;

        let root = {
            let appender_map = appenders
                .iter()
                .enumerate()
                .map(|(i, appender)| {
                    (&appender.name, i)
                })
                .collect::<HashMap<_, _>>();

            let config::Root { level, appenders, .. } = root;
            let mut root = ConfiguredLogger {
                level: level,
                appenders: appenders
                    .into_iter()
                    .map(|appender| appender_map[appender].clone())
                    .collect(),
                children: vec![],
            };

            for logger in loggers {
                let appenders = logger.appenders
                    .into_iter()
                    .map(|appender| appender_map[appender])
                    .collect();
                root.add(&logger.name, appenders, logger.additive, logger.level);
            }

            root
        };

        let appenders = appenders.into_iter().map(|appender| appender.appender).collect();

        SharedLogger {
            root: root,
            appenders: appenders,
        }
    }
}

struct Logger {
    inner: Arc<Mutex<SharedLogger>>,
}

impl Logger {
    fn new(config: config::Config) -> Logger {
        Logger {
            inner: Arc::new(Mutex::new(SharedLogger::new(config)))
        }
    }

    fn max_log_level(&self) -> LogLevelFilter {
        self.inner.lock().unwrap().root.max_log_level()
    }
}

impl log::Log for Logger {
    fn enabled(&self, level: LogLevel, module: &str) -> bool {
        self.inner.lock().unwrap().root.find(module).enabled(level)
    }

    fn log(&self, record: &log::LogRecord) {
        let shared = &mut *self.inner.lock().unwrap();
        shared.root.find(record.location().module_path).log(record, &mut shared.appenders);
    }
}

fn handle_error<E: error::Error+?Sized>(e: &E) {
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(&mut stderr, "{}", e);
}

/// Initializes the global logger with a log4rs logger configured by `config`.
pub fn init_config(config: config::Config) -> Result<(), SetLoggerError> {
    log::set_logger(|max_log_level| {
        let logger = Logger::new(config);
        max_log_level.set(logger.max_log_level());
        Box::new(logger)
    })
}

/// Initializes the global logger with a log4rs logger.
///
/// Configuration is read from a TOML file located at the provided path on the
/// filesystem and appenders are created from the provided `Creator`.
///
/// Any errors encountered when processing the configuration are reported to
/// stderr.
pub fn init_file<P: AsPath+?Sized>(path: &P, creator: Creator) -> Result<(), SetLoggerError> {
    log::set_logger(|max_log_level| {
        let path = path.as_path().to_path_buf();
        let mtime = match fs::metadata(&path) {
            Ok(metadata) => metadata.modified(),
            Err(err) => {
                handle_error(&err);
                0
            }
        };
        let (refresh_rate, config) = match load_config(&path, &creator) {
            Ok(toml::Config { refresh_rate, config, .. }) => (refresh_rate, config),
            Err(err) => {
                handle_error(&*err);
                (None, config::Config::new(vec![],
                                           config::Root::new(LogLevelFilter::Off),
                                           vec![]).unwrap())
            }
        };
        let logger = Logger::new(config);
        max_log_level.set(logger.max_log_level());
        if let Some(refresh_rate) = refresh_rate {
            ConfigReloader::start(path, refresh_rate, mtime, creator, &logger);
        }
        Box::new(logger)
    })
}

fn load_config(path: &Path, creator: &Creator) -> Result<toml::Config, Box<error::Error>> {
    let mut file = try!(File::open(path));
    let mut s = String::new();
    try!(file.read_to_string(&mut s));
    Ok(try!(toml::parse(&s, creator)))
}

struct ConfigReloader {
    path: PathBuf,
    rate: Duration,
    mtime: u64,
    creator: Creator,
    shared: Arc<Mutex<SharedLogger>>,
}

impl ConfigReloader {
    fn start(path: PathBuf, rate: Duration, mtime: u64, creator: Creator, logger: &Logger) {
        let mut reloader = ConfigReloader {
            path: path,
            rate: rate,
            mtime: mtime,
            creator: creator,
            shared: logger.inner.clone(),
        };

        thread::Builder::new()
            .name("log4rs config refresh thread".to_string())
            .spawn(move || reloader.run())
            .unwrap();
    }

    fn run(&mut self) {
        loop {
            sleep(self.rate);

            let mtime = match fs::metadata(&self.path) {
                Ok(metadata) => metadata.modified(),
                Err(err) => {
                    handle_error(&err);
                    continue;
                }
            };

            if mtime == self.mtime {
                continue;
            }

            self.mtime = mtime;

            let config = match load_config(&self.path, &self.creator) {
                Ok(config) => config,
                Err(err) => {
                    handle_error(&*err);
                    continue;
                }
            };
            let toml::Config { refresh_rate, config, ..  } = config;

            let shared = SharedLogger::new(config);
            *self.shared.lock().unwrap() = shared;

            match refresh_rate {
                Some(rate) => self.rate = rate,
                None => return,
            }
        }
    }
}

#[cfg(test)]
mod test {
    use log::{LogLevel, LogLevelFilter, Log};

    use super::*;

    #[test]
    fn enabled() {
        let appenders = vec![];
        let root = config::Root::new(LogLevelFilter::Debug);
        let loggers = vec![
            config::Logger::new("foo::bar".to_string(), LogLevelFilter::Trace),
            config::Logger::new("foo::bar::baz".to_string(), LogLevelFilter::Off),
            config::Logger::new("foo::baz::buz".to_string(), LogLevelFilter::Error),
        ];
        let config = config::Config::new(appenders, root, loggers).unwrap();

        let logger = super::Logger::new(config);

        assert!(logger.enabled(LogLevel::Warn, "bar"));
        assert!(!logger.enabled(LogLevel::Trace, "bar"));
        assert!(logger.enabled(LogLevel::Debug, "foo"));
        assert!(logger.enabled(LogLevel::Trace, "foo::bar"));
        assert!(!logger.enabled(LogLevel::Error, "foo::bar::baz"));
        assert!(logger.enabled(LogLevel::Debug, "foo::bar::bazbuz"));
        assert!(!logger.enabled(LogLevel::Error, "foo::bar::baz::buz"));
        assert!(!logger.enabled(LogLevel::Warn, "foo::baz::buz"));
        assert!(!logger.enabled(LogLevel::Warn, "foo::baz::buz::bar"));
        assert!(logger.enabled(LogLevel::Error, "foo::baz::buz::bar"));
    }
}