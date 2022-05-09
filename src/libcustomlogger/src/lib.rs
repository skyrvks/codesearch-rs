/// custom logger
extern crate chrono;
extern crate log;

use chrono::Local;
use log::{LevelFilter, Log, Metadata, Record, SetLoggerError};

static LOGGER: Logger = Logger;

pub struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }
    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let now = Local::now();
            let now_time = now.format("%Y/%m/%d %H:%M:%S");
            println!("{} {}", now_time, record.args());
        }
    }
    fn flush(&self) {}
}

pub fn init(level: LevelFilter) -> Result<(), SetLoggerError> {
    log::set_logger(&LOGGER)?;
    log::set_max_level(level);
    Ok(())
}
