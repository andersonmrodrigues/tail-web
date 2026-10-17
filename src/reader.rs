use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};

/// Size of each backward read. Large enough that a typical `tail -n 10` on a
/// log file needs a single read, small enough to keep memory flat.
const BLOCK_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
    /// The last N units.
    Last(u64),
    /// Everything starting at unit N (1-based; 0 behaves like 1).
    From(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Lines(Count),
    Bytes(Count),
}

/// Tails a seekable input, such as a regular file. `Last` modes never read
/// more than they print (plus one partial block), whatever the input size.
///
/// The input is read from its current position, so a file descriptor that was
/// partly consumed before being handed over is honoured, as GNU tail does.
pub fn tail_seekable<R: Read + Seek, W: Write>(
    input: &mut R,
    mode: Mode,
    out: &mut W,
) -> io::Result<()> {
    let start = input.stream_position()?;
    let end = input.seek(SeekFrom::End(0))?.max(start);

    let from = match mode {
        Mode::Lines(Count::Last(n)) => last_lines_offset(input, start, end, n, BLOCK_SIZE)?,
        Mode::Bytes(Count::Last(n)) => end.saturating_sub(n).max(start),
        Mode::Bytes(Count::From(n)) => start.saturating_add(n.saturating_sub(1)).min(end),
        Mode::Lines(Count::From(n)) => {
            input.seek(SeekFrom::Start(start))?;
            return copy_from_line(input, n, out);
        }
    };

    // Copy up to the size observed above, so a file that keeps growing while
    // we read does not turn a bounded tail into an unbounded one.
    input.seek(SeekFrom::Start(from))?;
    io::copy(&mut input.take(end - from), out)?;
    Ok(())
}

/// Tails an input that cannot seek, such as a pipe. `Last` modes buffer only
/// the units that may still be printed.
pub fn tail_stream<R: Read, W: Write>(input: R, mode: Mode, out: &mut W) -> io::Result<()> {
    match mode {
        Mode::Lines(Count::Last(n)) => last_lines_stream(input, n, out),
        Mode::Bytes(Count::Last(n)) => last_bytes_stream(input, n, out),
        Mode::Lines(Count::From(n)) => copy_from_line(input, n, out),
        Mode::Bytes(Count::From(n)) => {
            let mut input = input;
            let skip = n.saturating_sub(1);
            let skipped = io::copy(&mut (&mut input).take(skip), &mut io::sink())?;
            if skipped == skip {
                io::copy(&mut input, out)?;
            }
            Ok(())
        }
    }
}

/// Finds the offset where the last `n` lines of `[start, end)` begin by
/// reading backward one block at a time. A newline in the very last byte ends
/// the final line rather than starting an empty one, so it is not counted.
fn last_lines_offset<R: Read + Seek>(
    input: &mut R,
    start: u64,
    end: u64,
    n: u64,
    block_size: usize,
) -> io::Result<u64> {
    if n == 0 {
        return Ok(end);
    }

    let mut buf = vec![0; block_size];
    let mut pos = end;
    let mut found = 0;

    while pos > start {
        let len = (pos - start).min(block_size as u64) as usize;
        pos -= len as u64;
        input.seek(SeekFrom::Start(pos))?;
        input.read_exact(&mut buf[..len])?;

        for (i, &byte) in buf[..len].iter().enumerate().rev() {
            let offset = pos + i as u64;
            if byte == b'\n' && offset != end - 1 {
                found += 1;
                if found == n {
                    return Ok(offset + 1);
                }
            }
        }
    }
    Ok(start)
}

fn copy_from_line<R: Read, W: Write>(input: R, n: u64, out: &mut W) -> io::Result<()> {
    let mut reader = BufReader::with_capacity(BLOCK_SIZE, input);
    let mut skipped = Vec::new();
    for _ in 1..n {
        skipped.clear();
        if reader.read_until(b'\n', &mut skipped)? == 0 {
            return Ok(());
        }
    }
    io::copy(&mut reader, out)?;
    Ok(())
}

fn last_lines_stream<R: Read, W: Write>(input: R, n: u64, out: &mut W) -> io::Result<()> {
    if n == 0 {
        return Ok(());
    }

    let mut reader = BufReader::with_capacity(BLOCK_SIZE, input);
    let mut lines: VecDeque<Vec<u8>> = VecDeque::new();
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line)? > 0 {
        lines.push_back(std::mem::take(&mut line));
        if lines.len() as u64 > n {
            // Reuse the evicted line's allocation for the next read.
            line = lines.pop_front().unwrap_or_default();
            line.clear();
        }
    }

    for line in &lines {
        out.write_all(line)?;
    }
    Ok(())
}

fn last_bytes_stream<R: Read, W: Write>(mut input: R, n: u64, out: &mut W) -> io::Result<()> {
    if n == 0 {
        return Ok(());
    }

    // Keep at most 2n bytes buffered and trim back to n, so trimming stays
    // amortised O(1) per byte instead of shifting the buffer on every read.
    let keep = usize::try_from(n).unwrap_or(usize::MAX);
    let mut window = Vec::new();
    let mut chunk = vec![0; BLOCK_SIZE];
    loop {
        let read = match input.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        window.extend_from_slice(&chunk[..read]);
        if window.len() > keep.saturating_mul(2) {
            window.drain(..window.len() - keep);
        }
    }

    let from = window.len().saturating_sub(keep);
    out.write_all(&window[from..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::io::Cursor;

    /// Reference implementation: read everything, then slice.
    fn naive(data: &[u8], mode: Mode) -> Vec<u8> {
        let lines: Vec<&[u8]> = data.split_inclusive(|&b| b == b'\n').collect();
        let pick = |count: Count, len: usize| match count {
            Count::Last(n) => len.saturating_sub(n as usize),
            Count::From(n) => (n.max(1) as usize - 1).min(len),
        };
        match mode {
            Mode::Lines(count) => lines[pick(count, lines.len())..].concat(),
            Mode::Bytes(count) => data[pick(count, data.len())..].to_vec(),
        }
    }

    fn seekable(data: &[u8], mode: Mode) -> Vec<u8> {
        let mut out = Vec::new();
        tail_seekable(&mut Cursor::new(data), mode, &mut out).unwrap();
        out
    }

    fn stream(data: &[u8], mode: Mode) -> Vec<u8> {
        let mut out = Vec::new();
        tail_stream(data, mode, &mut out).unwrap();
        out
    }

    fn both(data: &[u8], mode: Mode) -> Vec<u8> {
        let a = seekable(data, mode);
        assert_eq!(a, stream(data, mode), "seekable and stream paths disagree");
        a
    }

    #[test]
    fn last_lines() {
        let data = b"one\ntwo\nthree\n";
        assert_eq!(both(data, Mode::Lines(Count::Last(2))), b"two\nthree\n");
        assert_eq!(both(data, Mode::Lines(Count::Last(0))), b"");
        assert_eq!(both(data, Mode::Lines(Count::Last(99))), data);
    }

    #[test]
    fn last_line_without_trailing_newline() {
        assert_eq!(both(b"one\ntwo", Mode::Lines(Count::Last(1))), b"two");
        assert_eq!(both(b"one\ntwo", Mode::Lines(Count::Last(2))), b"one\ntwo");
    }

    #[test]
    fn empty_lines_count() {
        assert_eq!(both(b"\n\n\n", Mode::Lines(Count::Last(2))), b"\n\n");
    }

    #[test]
    fn empty_input() {
        for mode in [
            Mode::Lines(Count::Last(10)),
            Mode::Lines(Count::From(1)),
            Mode::Bytes(Count::Last(10)),
            Mode::Bytes(Count::From(1)),
        ] {
            assert_eq!(both(b"", mode), b"", "{mode:?}");
        }
    }

    #[test]
    fn from_line_and_byte() {
        let data = b"one\ntwo\nthree\n";
        assert_eq!(both(data, Mode::Lines(Count::From(2))), b"two\nthree\n");
        assert_eq!(both(data, Mode::Lines(Count::From(0))), data);
        assert_eq!(both(data, Mode::Lines(Count::From(9))), b"");
        assert_eq!(both(data, Mode::Bytes(Count::From(5))), b"two\nthree\n");
        assert_eq!(both(data, Mode::Bytes(Count::From(99))), b"");
    }

    #[test]
    fn last_bytes() {
        assert_eq!(both(b"hello world", Mode::Bytes(Count::Last(5))), b"world");
        assert_eq!(both(b"hi", Mode::Bytes(Count::Last(5))), b"hi");
    }

    #[test]
    fn binary_data_passes_through() {
        let data: Vec<u8> = (0..=255u8).cycle().take(2000).collect();
        assert_eq!(
            both(&data, Mode::Lines(Count::Last(3))),
            naive(&data, Mode::Lines(Count::Last(3)))
        );
    }

    #[test]
    fn line_longer_than_a_block() {
        let mut data = b"head\n".to_vec();
        data.extend(std::iter::repeat_n(b'x', BLOCK_SIZE * 3));
        data.push(b'\n');
        assert_eq!(both(&data, Mode::Lines(Count::Last(1))), &data[5..]);
    }

    #[test]
    fn honours_initial_position() {
        let mut input = Cursor::new(b"skip\nkeep1\nkeep2\n".to_vec());
        input.set_position(5);
        let mut out = Vec::new();
        tail_seekable(&mut input, Mode::Lines(Count::Last(5)), &mut out).unwrap();
        assert_eq!(out, b"keep1\nkeep2\n");
    }

    #[test]
    fn reads_only_the_tail_of_large_inputs() {
        /// Counts bytes read so the test can assert on I/O, not just output.
        struct Metered<R> {
            inner: R,
            read: u64,
        }
        impl<R: Read> Read for Metered<R> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.inner.read(buf)?;
                self.read += n as u64;
                Ok(n)
            }
        }
        impl<R: Seek> Seek for Metered<R> {
            fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
                self.inner.seek(pos)
            }
        }

        let data = b"0123456789abcdef\n".repeat(1 << 16); // ~1.1 MB
        let mut input = Metered {
            inner: Cursor::new(&data),
            read: 0,
        };
        let mut out = Vec::new();
        tail_seekable(&mut input, Mode::Lines(Count::Last(10)), &mut out).unwrap();

        assert_eq!(out, naive(&data, Mode::Lines(Count::Last(10))));
        assert!(
            input.read <= (BLOCK_SIZE + out.len()) as u64,
            "read {} bytes",
            input.read
        );
    }

    fn any_mode() -> impl Strategy<Value = Mode> {
        let count = prop_oneof![
            (0..40u64).prop_map(Count::Last),
            (0..40u64).prop_map(Count::From)
        ];
        prop_oneof![
            count.clone().prop_map(Mode::Lines),
            count.prop_map(Mode::Bytes)
        ]
    }

    proptest! {
        #[test]
        fn matches_naive(
            data in prop::collection::vec(prop_oneof![Just(b'\n'), Just(b'a'), any::<u8>()], 0..300),
            mode in any_mode(),
        ) {
            let expected = naive(&data, mode);
            prop_assert_eq!(&seekable(&data, mode), &expected);
            prop_assert_eq!(&stream(&data, mode), &expected);
        }

        #[test]
        fn block_size_does_not_change_result(
            data in prop::collection::vec(prop_oneof![Just(b'\n'), Just(b'a')], 0..200),
            n in 0..30u64,
            block_size in 1..16usize,
        ) {
            let end = data.len() as u64;
            let small = last_lines_offset(&mut Cursor::new(&data), 0, end, n, block_size).unwrap();
            let large = last_lines_offset(&mut Cursor::new(&data), 0, end, n, BLOCK_SIZE).unwrap();
            prop_assert_eq!(small, large);
        }
    }
}
