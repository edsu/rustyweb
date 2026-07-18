//! Read a remote WACZ over HTTP range requests as a `Read + Seek` stream, so the
//! `zip` crate and the CDX-guided extractor can operate on it **without
//! downloading the whole file**. Only the bytes actually needed — the ZIP
//! central directory, a few local headers, the CDX, and the selected page
//! records — are fetched. This is the same primitive wabac.js uses for replay.

use std::io::{self, Read, Seek, SeekFrom};

use anyhow::{Context, Result};

/// Read-ahead window: small reads (ZIP headers, CDX lines) are amortized into
/// one request; a read larger than this fetches exactly what's asked.
const CHUNK: u64 = 256 * 1024;

/// The end of a ZIP holds the EOCD and the central directory, which the reader
/// touches repeatedly (once per entry) while also reading local headers scattered
/// across the file. Cache this many bytes of the tail once so those end-of-file
/// reads never re-fetch and don't get evicted by the rolling buffer.
const TAIL_CACHE: u64 = 1024 * 1024;

/// Fetches byte ranges of a resource. Abstracted from the buffering/seek logic
/// so that logic can be unit-tested against an in-memory source.
pub trait RangeFetch {
    fn total_len(&self) -> u64;
    /// Fetch bytes `[start, end)` (end exclusive); `end <= total_len`.
    fn fetch(&self, start: u64, end: u64) -> io::Result<Vec<u8>>;
}

/// A `Read + Seek` over a [`RangeFetch`] with a single read-ahead buffer.
pub struct RangeReader<F: RangeFetch> {
    fetch: F,
    pos: u64,
    // Rolling forward buffer for reads in the body of the file.
    buf: Vec<u8>,
    buf_start: u64,
    // Cached tail (end-of-file region: EOCD + central directory), fetched once.
    tail: Option<Vec<u8>>,
    tail_start: u64,
}

impl<F: RangeFetch> RangeReader<F> {
    pub fn new(fetch: F) -> Self {
        let tail_start = fetch.total_len().saturating_sub(TAIL_CACHE);
        Self { fetch, pos: 0, buf: Vec::new(), buf_start: 0, tail: None, tail_start }
    }

    pub fn total_len(&self) -> u64 {
        self.fetch.total_len()
    }

    /// Offset into `buf` for `pos`, if `pos` is currently buffered.
    fn buffered_offset(&self, pos: u64) -> Option<usize> {
        let end = self.buf_start + self.buf.len() as u64;
        (pos >= self.buf_start && pos < end).then(|| (pos - self.buf_start) as usize)
    }
}

impl<F: RangeFetch> Read for RangeReader<F> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let total = self.fetch.total_len();
        if self.pos >= total || out.is_empty() {
            return Ok(0);
        }

        // Reads in the tail region (EOCD + central directory) come from a cache
        // fetched once, so they don't thrash against scattered body reads.
        if self.pos >= self.tail_start {
            if self.tail.is_none() {
                self.tail = Some(self.fetch.fetch(self.tail_start, total)?);
            }
            let tail = self.tail.as_ref().unwrap();
            let off = (self.pos - self.tail_start) as usize;
            let n = (tail.len() - off).min(out.len());
            out[..n].copy_from_slice(&tail[off..off + n]);
            self.pos += n as u64;
            return Ok(n);
        }

        let off = match self.buffered_offset(self.pos) {
            Some(o) => o,
            None => {
                // Fetch a window at pos: at least the requested size, otherwise a
                // read-ahead chunk, capped at the tail (or file end).
                let want = (out.len() as u64).max(CHUNK).min(total - self.pos);
                let end = (self.pos + want).min(total);
                self.buf = self.fetch.fetch(self.pos, end)?;
                self.buf_start = self.pos;
                0
            }
        };
        let avail = &self.buf[off..];
        let n = avail.len().min(out.len());
        out[..n].copy_from_slice(&avail[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<F: RangeFetch> Seek for RangeReader<F> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let (base, delta) = match from {
            SeekFrom::Start(p) => (0i64, p as i64),
            SeekFrom::End(p) => (self.fetch.total_len() as i64, p),
            SeekFrom::Current(p) => (self.pos as i64, p),
        };
        let np = base + delta;
        if np < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = np as u64;
        Ok(self.pos)
    }
}

/// A [`RangeFetch`] backed by HTTP range GETs via `ureq`.
pub struct HttpFetch {
    agent: ureq::Agent,
    url: String,
    len: u64,
}

impl HttpFetch {
    /// Probe the resource for its total size and confirm the server honors range
    /// requests (a `206` with `Content-Range`). Errors if it doesn't — the
    /// caller can then fall back to downloading.
    pub fn open(url: &str) -> Result<Self> {
        let agent = ureq::agent();
        let resp = agent
            .get(url)
            .set("Range", "bytes=0-0")
            .call()
            .with_context(|| format!("HTTP range probe of {url}"))?;
        if resp.status() != 206 {
            anyhow::bail!(
                "{url} did not honor a range request (status {}); the server must \
                 support HTTP range requests to stream-index it — use --download instead",
                resp.status()
            );
        }
        // Content-Range: "bytes 0-0/<total>"
        let len = resp
            .header("Content-Range")
            .and_then(|cr| cr.rsplit('/').next())
            .map(str::trim)
            .and_then(|n| n.parse::<u64>().ok())
            .with_context(|| format!("no total length in Content-Range from {url}"))?;
        Ok(Self { agent, url: url.to_string(), len })
    }
}

impl RangeFetch for HttpFetch {
    fn total_len(&self) -> u64 {
        self.len
    }

    fn fetch(&self, start: u64, end: u64) -> io::Result<Vec<u8>> {
        let resp = self
            .agent
            .get(&self.url)
            .set("Range", &format!("bytes={}-{}", start, end - 1))
            .call()
            .map_err(|e| io::Error::other(format!("range GET of {}: {e}", self.url)))?;
        let mut v = Vec::with_capacity((end - start) as usize);
        resp.into_reader().read_to_end(&mut v)?;
        Ok(v)
    }
}

/// Open a remote WACZ as a `Read + Seek` HTTP range stream.
pub fn open_remote(url: &str) -> Result<RangeReader<HttpFetch>> {
    Ok(RangeReader::new(HttpFetch::open(url)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory RangeFetch for testing the buffering/seek logic.
    struct MemFetch(Vec<u8>);
    impl RangeFetch for MemFetch {
        fn total_len(&self) -> u64 {
            self.0.len() as u64
        }
        fn fetch(&self, start: u64, end: u64) -> io::Result<Vec<u8>> {
            Ok(self.0[start as usize..end as usize].to_vec())
        }
    }

    /// Read exactly up to `n` bytes (looping over partial reads).
    fn read_n<R: Read>(r: &mut R, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut tmp = vec![0u8; n];
        while out.len() < n {
            let got = r.read(&mut tmp[..n - out.len()]).unwrap();
            if got == 0 {
                break;
            }
            out.extend_from_slice(&tmp[..got]);
        }
        out
    }

    #[test]
    fn range_reader_reads_and_seeks_like_a_cursor() {
        // 5000 bytes crosses the (much larger) chunk boundary logic; use a size
        // below CHUNK plus a case that reads more than CHUNK by shrinking CHUNK
        // conceptually via a large read.
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        let mut rr = RangeReader::new(MemFetch(data.clone()));
        let mut cur = std::io::Cursor::new(data.clone());

        for &(pos, n) in &[(0u64, 10usize), (4990, 50), (1234, 300), (0, 5000), (2500, 2500)] {
            rr.seek(SeekFrom::Start(pos)).unwrap();
            cur.seek(SeekFrom::Start(pos)).unwrap();
            assert_eq!(read_n(&mut rr, n), read_n(&mut cur, n), "at pos {pos} len {n}");
        }

        // SeekFrom::End and Current.
        assert_eq!(rr.seek(SeekFrom::End(-5)).unwrap(), 4995);
        assert_eq!(read_n(&mut rr, 100), data[4995..].to_vec());
        rr.seek(SeekFrom::Start(100)).unwrap();
        rr.seek(SeekFrom::Current(50)).unwrap();
        assert_eq!(read_n(&mut rr, 10), data[150..160].to_vec());
    }

    #[test]
    fn range_reader_large_read_exceeding_chunk() {
        // A read larger than CHUNK must still work (fetches exactly what's asked).
        let n = (CHUNK * 2 + 123) as usize;
        let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let mut rr = RangeReader::new(MemFetch(data.clone()));
        assert_eq!(read_n(&mut rr, n), data);
    }

    #[test]
    fn range_reader_tail_and_body_regions_match_cursor() {
        // Larger than the tail cache, so both the tail and the rolling body
        // buffer are exercised.
        let n = (TAIL_CACHE + 300_000) as usize;
        let data: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let mut rr = RangeReader::new(MemFetch(data.clone()));
        let mut cur = std::io::Cursor::new(data.clone());
        for &(pos, len) in &[
            (0u64, 1000usize),
            (500_000, 4096),
            (n as u64 - 50, 50),          // tail
            (n as u64 - 300_000, 200_000), // spans body into tail
        ] {
            rr.seek(SeekFrom::Start(pos)).unwrap();
            cur.seek(SeekFrom::Start(pos)).unwrap();
            assert_eq!(read_n(&mut rr, len), read_n(&mut cur, len), "pos {pos} len {len}");
        }
    }

    /// Counts fetches so we can assert the tail cache prevents re-fetching.
    struct CountingFetch {
        data: Vec<u8>,
        count: std::rc::Rc<std::cell::Cell<usize>>,
    }
    impl RangeFetch for CountingFetch {
        fn total_len(&self) -> u64 {
            self.data.len() as u64
        }
        fn fetch(&self, start: u64, end: u64) -> io::Result<Vec<u8>> {
            self.count.set(self.count.get() + 1);
            Ok(self.data[start as usize..end as usize].to_vec())
        }
    }

    #[test]
    fn tail_cache_avoids_thrashing_between_end_and_body() {
        // Reproduces the ZIP pattern: read the central directory (end) and a
        // local header (body), alternating. Each region must be fetched once.
        let n = (TAIL_CACHE + 4_000_000) as usize;
        let count = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let mut rr = RangeReader::new(CountingFetch { data: vec![9u8; n], count: count.clone() });
        for _ in 0..20 {
            rr.seek(SeekFrom::End(-100)).unwrap();
            read_n(&mut rr, 40); // "central directory" read (tail)
            rr.seek(SeekFrom::Start(2_000_000)).unwrap();
            read_n(&mut rr, 30); // "local header" read (body)
        }
        assert!(
            count.get() <= 3,
            "tail cache should make ~2 fetches, not 40; got {}",
            count.get()
        );
    }
}
