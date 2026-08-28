use std::io::{self, Read};

const INITIAL_CAPACITY_LIMIT: u64 = 64 * 1024;

#[derive(Debug)]
pub(crate) enum BoundedReadError {
    Io(io::Error),
    LimitExceeded,
}

pub(crate) fn read_to_end(
    reader: &mut (impl Read + ?Sized),
    limit: u64,
) -> Result<Vec<u8>, BoundedReadError> {
    let read_limit = limit.saturating_add(1);
    let capacity = usize::try_from(limit.min(INITIAL_CAPACITY_LIMIT)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(capacity);
    reader
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(BoundedReadError::Io)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit {
        return Err(BoundedReadError::LimitExceeded);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::io::{self, Read};

    use super::{BoundedReadError, read_to_end};

    struct GrowingReader {
        reads: usize,
    }

    impl Read for GrowingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            buffer.fill(b'x');
            Ok(buffer.len())
        }
    }

    #[test]
    fn bounded_read_growing_stream_stops_after_limit_plus_one() {
        let mut reader = GrowingReader { reads: 0 };
        let result = read_to_end(&mut reader, 32);

        assert!(matches!(result, Err(BoundedReadError::LimitExceeded)));
        assert!(reader.reads <= 2);
    }
}
