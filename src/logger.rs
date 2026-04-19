use log::{LevelFilter, Metadata, Record};
struct KernelLogger;
impl log::Log for KernelLogger {
    fn enabled(&self, meta: &Metadata) -> bool {
        meta.level() <= log::Level::Info
    }
    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            crate::serial_println!("[{}] {}", record.target(), record.args());
        }
    }
    fn flush(&self) {}
}
static LOGGER: KernelLogger = KernelLogger;
pub fn init() {
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(LevelFilter::Info);
}
