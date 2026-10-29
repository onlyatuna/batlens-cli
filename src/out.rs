//! Buffers `println!` output and writes it once at exit, so a closed pipe
//! (`batlens report | head`) is handled in one place.

use std::io::{self, Write};
use std::sync::Mutex;

static BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());

#[doc(hidden)]
pub fn push(args: std::fmt::Arguments) {
    let mut b = BUF.lock().unwrap_or_else(|e| e.into_inner());
    let _ = b.write_fmt(args);
}

/// Shadows `std::println!` for the whole crate; output is flushed by [`flush`].
macro_rules! println {
    () => {
        $crate::out::push(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::out::push(format_args!("{}\n", format_args!($($arg)*)))
    };
}

/// Writes `data` to `w`. A closed pipe is not an error: the reader no longer wants data.
fn write_out<W: Write>(w: &mut W, data: &[u8]) -> io::Result<()> {
    match w.write_all(data).and_then(|_| w.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        r => r,
    }
}

/// Writes all buffered output to stdout once.
pub fn flush() -> io::Result<()> {
    let data = std::mem::take(&mut *BUF.lock().unwrap_or_else(|e| e.into_inner()));
    write_out(&mut io::stdout().lock(), &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailWriter(io::ErrorKind);
    impl Write for FailWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_broken_pipe_is_ok_other_errors_propagate() {
        assert!(write_out(&mut FailWriter(io::ErrorKind::BrokenPipe), b"x").is_ok());
        assert!(write_out(&mut FailWriter(io::ErrorKind::PermissionDenied), b"x").is_err());
    }
}
